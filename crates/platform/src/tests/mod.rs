use crate as platform;
use crate::*;
use compiler_core::{CompiledPlan, PermissionCeiling, digest, parse_and_validate};
use coordination::{ClaimRef, ClaimSnapshotV1, ClaimStore};
use deploy::{ClaimState, ProtectedClaim, ResourceIdentity};
use http::{Request, Response};
use http_body_util::BodyExt;
use kube::{Client, client::Body};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    convert::Infallible,
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex, OnceLock},
};
use tower::service_fn;

#[cfg(feature = "deployer")]
mod capacity_accounting;
#[cfg(feature = "deployer")]
mod capacity_topology;
mod event_capacity;

const REGISTRY_UID: &str = "11111111-1111-4111-8111-111111111111";
const GENERATION_A: &str = "gceirceircfardairceirceirce-1";
const GENERATION_B: &str = "gceirceircfardairceirceirce-2";
const TRANSITION_A: &str = "tceirceircfardairceirceirce-1";
const TRANSITION_B: &str = "tceirceircfardairceirceirce-2";

#[derive(Clone)]
struct LifecycleRegistry {
    registry: NamespaceRegistry,
    plans: BTreeMap<String, deploy::TransitionPlan>,
}

impl Deref for LifecycleRegistry {
    type Target = NamespaceRegistry;

    fn deref(&self) -> &Self::Target {
        &self.registry
    }
}

impl DerefMut for LifecycleRegistry {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.registry
    }
}

impl LifecycleRegistry {
    fn plan(&self, transition: &str) -> std::result::Result<&deploy::TransitionPlan, &'static str> {
        self.plans.get(transition).ok_or("fixture plan missing")
    }

    #[cfg(feature = "deployer")]
    fn admit(&mut self, transition: &str) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let next =
            fixture_review_store().seed(crate::project_review_metadata(self.plan(transition)?)?)?;
        let previous = self
            .transition(transition)?
            .inputs
            .previous
            .as_ref()
            .map(|prior| {
                self.transition(&prior.allocation.transition_id)
                    .map(|stored| stored.next_review_metadata.clone())
            })
            .transpose()?;
        let stored = self
            .transitions
            .get_mut(transition)
            .ok_or("fixture transition missing")?;
        stored.next_review_metadata = next;
        stored.previous_review_metadata = previous;
        let profile = stored.capacity.profile.clone();
        let capacity =
            crate::completion_envelope::calculate(self, self.plan(transition)?, &profile)?;
        self.transitions
            .get_mut(transition)
            .ok_or("fixture transition missing")?
            .capacity = capacity;
        Ok(())
    }

    fn bind(
        &self,
        client: Client,
        control_namespace: &str,
        workload_namespace: &str,
        cluster_binding: &str,
        transition: &str,
    ) -> std::result::Result<PlatformClient, Box<dyn std::error::Error>> {
        let stored = self.transition(transition)?;
        Ok(platform_client(
            client,
            control_namespace,
            workload_namespace,
            cluster_binding,
        )?
        .bind_test_plan(
            self.plan(transition)?,
            stored.previous_writer_checkpoint.as_ref(),
            &stored.capacity,
            &fixture_review_store()
                .read(&stored.next_review_metadata)?
                .ok_or("fixture next review missing")?,
            stored
                .previous_review_metadata
                .as_ref()
                .map(|reference| {
                    fixture_review_store()
                        .read(reference)?
                        .ok_or(coordination::Error::Invalid(
                            "fixture previous review missing",
                        ))
                })
                .transpose()?
                .as_ref(),
        )?)
    }
}

mod audit_retention;
#[path = "../../tests/support/audit.rs"]
mod audit_support;
mod compact_state;
#[cfg(feature = "deployer")]
mod qualified_writer;
#[cfg(feature = "deployer")]
mod review_bindings;
#[cfg(feature = "deployer")]
mod review_upgrade;
#[path = "../../tests/support/review.rs"]
mod review_support;

type Calls = Arc<Mutex<Vec<(String, String, Value)>>>;

#[derive(Default)]
struct MemoryClaimStore {
    rows: Mutex<BTreeMap<String, ClaimSnapshotV1>>,
}

struct LyingClaimStore {
    snapshot: ClaimSnapshotV1,
}

struct CorruptClaimStore {
    snapshot: ClaimSnapshotV1,
}

#[derive(Default)]
struct MemoryInventoryStore {
    rows: Mutex<BTreeMap<String, inventory::InventorySnapshot>>,
}

impl MemoryInventoryStore {
    fn key(scope: &inventory::InventoryScope, snapshot_id: &str) -> String {
        format!(
            "{}\0{}\0{}\0{snapshot_id}",
            scope.cluster, scope.namespace, scope.controller
        )
    }

    fn seed(
        &self,
        scope: &inventory::InventoryScope,
        snapshot: &inventory::InventorySnapshot,
    ) -> inventory::Result<()> {
        let id = inventory::snapshot_id(scope, snapshot)?;
        self.rows
            .lock()
            .map_err(|_| inventory::Error::Backend("test inventory lock"))?
            .insert(Self::key(scope, &id), snapshot.clone());
        Ok(())
    }
}

#[async_trait::async_trait]
impl inventory::InventoryStore for MemoryInventoryStore {
    async fn load(
        &self,
        _: &inventory::InventoryScope,
    ) -> inventory::Result<Option<inventory::VersionedInventory>> {
        Ok(None)
    }

    async fn read_snapshot(
        &self,
        scope: &inventory::InventoryScope,
        snapshot_id: &str,
    ) -> inventory::Result<Option<inventory::InventorySnapshot>> {
        let snapshot = self
            .rows
            .lock()
            .map_err(|_| inventory::Error::Backend("test inventory lock"))?
            .get(&Self::key(scope, snapshot_id))
            .cloned();
        if let Some(snapshot) = &snapshot
            && inventory::snapshot_id(scope, snapshot)? != snapshot_id
        {
            return Err(inventory::Error::Invalid("test inventory integrity"));
        }
        Ok(snapshot)
    }

    async fn publish(
        &self,
        scope: &inventory::InventoryScope,
        _: Option<&str>,
        snapshot: &inventory::InventorySnapshot,
    ) -> inventory::Result<inventory::VersionedInventory> {
        self.seed(scope, snapshot)?;
        Ok(inventory::VersionedInventory {
            revision: "1".into(),
            snapshot_id: inventory::snapshot_id(scope, snapshot)?,
            snapshot: snapshot.clone(),
        })
    }
}

async fn reserve_test_transition(
    platform: &PlatformClient,
    request: &deploy::DeploymentRequest,
    executor: &str,
) -> platform::Result<Versioned<NamespaceRegistry>> {
    let inventory = MemoryInventoryStore::default();
    if let Some(previous) = request.predecessor()? {
        inventory.seed(&previous.scope, &previous.inventory)?;
    }
    Box::pin(platform.reserve_authenticated(request, executor, &inventory, None)).await
}

impl MemoryClaimStore {
    fn seed(&self, snapshot: ClaimSnapshotV1) -> coordination::Result<ClaimRef> {
        let reference = snapshot.reference()?;
        let mut rows = self
            .rows
            .lock()
            .map_err(|_| coordination::Error::Backend("test claim store lock"))?;
        if let Some(existing) = rows.get(&reference.digest)
            && existing != &snapshot
        {
            return Err(coordination::Error::Invalid("test claim digest collision"));
        }
        rows.insert(reference.digest.clone(), snapshot);
        Ok(reference)
    }
}

#[async_trait::async_trait]
impl ClaimStore for MemoryClaimStore {
    async fn put_claims(&self, snapshot: &ClaimSnapshotV1) -> coordination::Result<ClaimRef> {
        self.seed(snapshot.clone())
    }

    async fn read_claims(
        &self,
        reference: &ClaimRef,
    ) -> coordination::Result<Option<ClaimSnapshotV1>> {
        reference.validate()?;
        let rows = self
            .rows
            .lock()
            .map_err(|_| coordination::Error::Backend("test claim store lock"))?;
        let Some(snapshot) = rows.get(&reference.digest) else {
            return Ok(None);
        };
        if snapshot.reference()? != *reference {
            return Err(coordination::Error::Invalid(
                "test claim reference mismatch",
            ));
        }
        Ok(Some(snapshot.clone()))
    }
}

#[async_trait::async_trait]
impl ClaimStore for LyingClaimStore {
    async fn put_claims(&self, snapshot: &ClaimSnapshotV1) -> coordination::Result<ClaimRef> {
        let mut reference = snapshot.reference()?;
        reference.digest = format!("sha256:{}", "0".repeat(64));
        Ok(reference)
    }

    async fn read_claims(
        &self,
        reference: &ClaimRef,
    ) -> coordination::Result<Option<ClaimSnapshotV1>> {
        if self.snapshot.reference()? == *reference {
            Ok(Some(self.snapshot.clone()))
        } else {
            Ok(None)
        }
    }
}

#[async_trait::async_trait]
impl ClaimStore for CorruptClaimStore {
    async fn put_claims(&self, snapshot: &ClaimSnapshotV1) -> coordination::Result<ClaimRef> {
        snapshot.reference()
    }

    async fn read_claims(&self, _: &ClaimRef) -> coordination::Result<Option<ClaimSnapshotV1>> {
        Ok(Some(self.snapshot.clone()))
    }
}

fn fixture_claim_store() -> &'static Arc<MemoryClaimStore> {
    static STORE: OnceLock<Arc<MemoryClaimStore>> = OnceLock::new();
    STORE.get_or_init(|| Arc::new(MemoryClaimStore::default()))
}

fn fixture_review_store() -> &'static Arc<review_support::MemoryReviewStore> {
    static STORE: OnceLock<Arc<review_support::MemoryReviewStore>> = OnceLock::new();
    STORE.get_or_init(|| Arc::new(review_support::MemoryReviewStore::default()))
}

fn platform_client(
    client: Client,
    control_namespace: &str,
    workload_namespace: &str,
    cluster_binding: &str,
) -> platform::Result<PlatformClient> {
    Ok(PlatformClient::new(
        client,
        control_namespace,
        workload_namespace,
        cluster_binding,
    )?
    .with_claim_store(fixture_claim_store().clone())
    .with_audit_store(Arc::new(audit_support::MemoryAuditStore::default()))
    .with_review_store(fixture_review_store().clone()))
}

fn claim_snapshot(registry: &NamespaceRegistry) -> platform::Result<ClaimSnapshotV1> {
    registry.claim_snapshot()
}

fn registry_object(
    registry: &NamespaceRegistry,
    uid: &str,
    resource_version: &str,
) -> std::result::Result<Value, Box<dyn std::error::Error>> {
    let snapshot = claim_snapshot(registry)?;
    let reference = fixture_claim_store().seed(snapshot)?;
    let state = registry.state(&reference)?;
    Ok(object(
        "reakt-registry-workload",
        &state,
        uid,
        resource_version,
    ))
}

fn hydrate_registry(
    encoded: &str,
) -> std::result::Result<NamespaceRegistry, Box<dyn std::error::Error>> {
    let state: NamespaceRegistryState = serde_json::from_str(encoded)?;
    let snapshot = fixture_claim_store()
        .rows
        .lock()
        .map_err(|_| "test claim store lock poisoned")?
        .get(&state.claim_snapshot.digest)
        .cloned()
        .ok_or("test claim snapshot missing")?;
    Ok(state.hydrate(&snapshot)?)
}

fn hydrated_registry(
    next_sequence: u64,
    claims: Vec<ProtectedClaim>,
    histories: BTreeMap<String, deploy::GenerationHistory>,
    active_writer_checkpoints: BTreeMap<String, WriterCheckpoint>,
    transitions: BTreeMap<String, StoredTransition>,
) -> std::result::Result<NamespaceRegistry, Box<dyn std::error::Error>> {
    let snapshot = ClaimSnapshotV1 {
        schema_version: coordination::CLAIM_SNAPSHOT_SCHEMA.to_owned(),
        scope: coordination::ClaimScope {
            registry_uid: REGISTRY_UID.to_owned(),
            cluster_binding: "cluster-a".to_owned(),
            control_namespace: "control".to_owned(),
            workload_namespace: "workload".to_owned(),
        },
        claims,
    };
    let reference = fixture_claim_store().seed(snapshot.clone())?;
    Ok(NamespaceRegistryState {
        audit_heads: BTreeMap::new(),
        registry_uid: REGISTRY_UID.to_owned(),
        next_sequence,
        schema_version: REGISTRY_SCHEMA_VERSION.to_owned(),
        cluster_binding: "cluster-a".to_owned(),
        workload_namespace: "workload".to_owned(),
        claim_snapshot: reference,
        histories,
        active_writer_checkpoints,
        transitions,
    }
    .hydrate(&snapshot)?)
}

fn transition_inputs(
    plan: &deploy::TransitionPlan,
    policy: &PolicyState,
) -> std::result::Result<TransitionInputs, Box<dyn std::error::Error>> {
    let domain = &plan.request.next.compiled.ir.domain;
    let claims = ClaimSnapshotV1 {
        schema_version: coordination::CLAIM_SNAPSHOT_SCHEMA.to_owned(),
        scope: coordination::ClaimScope {
            registry_uid: plan.request.registry.registry_uid.clone(),
            cluster_binding: domain.cluster_binding.clone(),
            control_namespace: domain.control_namespace.clone(),
            workload_namespace: domain.namespace.clone(),
        },
        claims: plan.request.registry.claims.clone(),
    };
    Ok(TransitionInputs::freeze(plan, claims.reference()?, policy)?)
}
fn registry_authority() -> Value {
    let mut authority = object(
        "reakt-registry-authority.control.workload",
        &json!({
            "schemaVersion":"reakt.dev/registry-authority/v1",
            "clusterBinding":"cluster-a", "controlNamespace":"control", "workloadNamespace":"workload",
            "registryUid":REGISTRY_UID,
        }),
        "authority-uid",
        "40",
    );
    authority["metadata"]["namespace"] = json!("reakt-platform");
    authority
}

fn wire(responses: Vec<Value>) -> (Client, Calls) {
    wire_with_authority(responses, registry_authority())
}

fn wire_with_authority(responses: Vec<Value>, authority: Value) -> (Client, Calls) {
    let replies = Arc::new(Mutex::new(VecDeque::from(responses)));
    let calls: Calls = Arc::default();
    let service = service_fn({
        let calls = calls.clone();
        move |request: Request<Body>| {
            let replies = replies.clone();
            let calls = calls.clone();
            let authority = authority.clone();
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
                    .push((
                        parts.method.to_string(),
                        parts.uri.to_string(),
                        data.clone(),
                    ));
                let mut reply = if parts.method == http::Method::GET
                    && parts
                        .uri
                        .path()
                        .ends_with("/configmaps/reakt-registry-authority.control.workload")
                {
                    authority
                } else {
                    replies
                        .lock()
                        .map_err(|_| std::io::Error::other("replies lock poisoned"))?
                        .pop_front()
                        .ok_or_else(|| std::io::Error::other("unexpected Kubernetes request"))?
                };
                let status = reply
                    .get("wireHttpStatus")
                    .and_then(Value::as_u64)
                    .map(u16::try_from)
                    .transpose()
                    .map_err(std::io::Error::other)?
                    .unwrap_or(200);
                if reply.get("wireHttpStatus").is_some() {
                    reply = reply
                        .get("body")
                        .cloned()
                        .ok_or_else(|| std::io::Error::other("wire response body missing"))?;
                }
                if reply == json!("echo-activation-create-large-rv") {
                    reply = data;
                    reply["metadata"]["uid"] = json!("activation-uid");
                    reply["metadata"]["resourceVersion"] = json!("9".repeat(129));
                } else if reply == json!("echo-put") {
                    reply = data;
                    reply["metadata"]["resourceVersion"] = json!("43");
                } else if reply == json!("echo-mutated-registry-put") {
                    reply = data;
                    let encoded = reply["data"]["state.json"]
                        .as_str()
                        .ok_or_else(|| std::io::Error::other("registry state missing"))?;
                    let mut state: Value =
                        serde_json::from_str(encoded).map_err(std::io::Error::other)?;
                    state["nextSequence"] = json!("3");
                    reply["data"]["state.json"] =
                        json!(serde_json::to_string(&state).map_err(std::io::Error::other)?);
                    reply["metadata"]["resourceVersion"] = json!("43");
                }
                Response::builder()
                    .status(status)
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

fn plan() -> CompiledPlan {
    parse_and_validate(&json!({"schemaVersion":"reakt.dev/ir/v1","testOnly":false,
        "domain":{"id":"example","namespace":"workload","controlNamespace":"control","clusterBinding":"cluster-a","policyRef":"platform/example","enforcement":"api-bound","budgets":{"maxResources":64,"maxInputs":64,"maxReactions":64,"maxObjectBytes":262_144,"maxQueueDepth":128,"maxConcurrentRequests":4}},
        "resources":[{"id":"config","apiVersion":"v1","kind":"ConfigMap","name":"config","namespace":"workload","desired":{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"config","namespace":"workload"},"data":{"value":"approved"}},"lifecycle":{"deletionPolicy":"orphan"},"source":{"file":"program.ts","line":1,"column":1}}],
        "inputs":[],"reactions":[],"provenance":{"compiler":"reakt/0.1","inputs":[{"path":"program.ts","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}]}
    }).to_string()).unwrap()
}

fn policy(plan: &CompiledPlan) -> PolicyState {
    let d = &plan.ir.domain;
    let ceiling = PermissionCeiling {
        policy_ref: d.policy_ref.clone(),
        domain_id: d.id.clone(),
        cluster_binding: d.cluster_binding.clone(),
        workload_namespace: d.namespace.clone(),
        control_namespace: d.control_namespace.clone(),
        capabilities: plan.capabilities.clone(),
        accepted_enforcement_gaps: plan.enforcement_gaps.clone(),
        accepted_indirect_risks: plan.indirect_risks.clone(),
    };
    PolicyState {
        schema_version: SCHEMA_VERSION.into(),
        runtime_ceiling: ceiling.clone(),
        cleanup_ceiling: ceiling,
    }
}
fn activation(plan: &CompiledPlan, policy: &PolicyState) -> ActivationState {
    ActivationState {
        schema_version: SCHEMA_VERSION.into(),
        binding: RuntimeBinding {
            domain_id: "example".into(),
            cluster_binding: "cluster-a".into(),
            generation_id: GENERATION_A.into(),
            build_digest: format!("sha256:{}", "a".repeat(64)),
            ir_digest: plan.ir_digest.clone(),
            policy_digest: digest(policy).unwrap(),
            capability_digest: digest(&plan.capabilities).unwrap(),
            service_account_name: "reakt-example-generation-a".into(),
            controller_image: format!("example.invalid/controller@sha256:{}", "b".repeat(64)),
        },
        status: ActivationStatus::Active,
        approved_claims: vec![ApprovedClaim {
            claim: ProtectedClaim {
                identity: ResourceIdentity {
                    api_group: String::new(),
                    resource: "configmaps".into(),
                    namespace: "workload".into(),
                    name: "config".into(),
                },
                domain_id: "example".into(),
                generation_id: GENERATION_A.into(),
                transition_id: TRANSITION_A.into(),
                state: ClaimState::Active,
                uid: None,
            },
            deletion_policy: compiler_core::DeletionPolicy::Orphan,
            allow_create: true,
        }],
        rollout_key_id: None,
    }
}
fn object(name: &str, value: &impl serde::Serialize, uid: &str, rv: &str) -> Value {
    json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":name,"namespace":"control","uid":uid,"resourceVersion":rv,"annotations":{"platform.example/keep":"true"}},"data":{"state.json":serde_json::to_string(value).unwrap()}})
}
fn inventory(activation: &ActivationState) -> RuntimeInventory {
    RuntimeInventory {
        schema_version: SCHEMA_VERSION.into(),
        binding: activation.binding.clone(),
        activation_uid: "activation-uid".into(),
        activation_resource_version: "20".into(),
        observations: vec![],
        conditions: vec![],
        drained: false,
    }
}

#[tokio::test]
async fn protected_activation_is_checked_against_real_policy_and_complete_claims() {
    let plan = plan();
    let policy = policy(&plan);
    let activation = activation(&plan, &policy);
    for corrupt in ["none", "claim", "policy"] {
        let mut actual = activation.clone();
        let mut actual_policy = policy.clone();
        if corrupt == "claim" {
            actual.approved_claims[0].claim.identity.name = "foreign".into();
        }
        if corrupt == "policy" {
            actual_policy
                .runtime_ceiling
                .accepted_indirect_risks
                .push("unapproved".into());
        }
        let (client, calls) = wire(vec![
            object("activation", &actual, "activation-uid", "20"),
            object("policy", &actual_policy, "policy-uid", "19"),
        ]);
        let authority =
            RuntimeAuthority::new(client, "control", activation.binding.clone()).unwrap();
        assert_eq!(authority.verify(&plan).await.is_ok(), corrupt == "none");
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .all(|(method, _, _)| method == "GET")
        );
    }
}

#[tokio::test]
async fn draining_blocks_mutation_authority_but_allows_bound_drain_inventory_cas() {
    let plan = plan();
    let policy = policy(&plan);
    let mut activation = activation(&plan, &policy);
    activation.status = ActivationStatus::Draining;
    let activation_object = object("activation", &activation, "activation-uid", "20");
    let (client, calls) = wire(vec![activation_object.clone()]);
    assert!(matches!(
        RuntimeAuthority::new(client, "control", activation.binding.clone())
            .unwrap()
            .verify(&plan)
            .await,
        Err(Error::Inactive)
    ));
    assert_eq!(calls.lock().unwrap().len(), 1);
    let previous = inventory(&activation);
    let mut next = previous.clone();
    next.drained = true;
    let previous_object = object("inventory", &previous, "inventory-uid", "42");
    let (client, calls) = wire(vec![activation_object, previous_object, json!("echo-put")]);
    let authority = RuntimeAuthority::new(client, "control", activation.binding).unwrap();
    let updated = authority
        .publish_drained_inventory(
            &Versioned {
                value: previous,
                uid: "inventory-uid".into(),
                resource_version: "42".into(),
            },
            next,
        )
        .await
        .unwrap();
    assert!(updated.value.drained);
    let calls = calls.lock().unwrap();
    assert_eq!(calls[2].0, "PUT");
    assert_eq!(calls[2].2["metadata"]["uid"], "inventory-uid");
    assert_eq!(calls[2].2["metadata"]["resourceVersion"], "42");
    assert_eq!(
        calls[2].2["metadata"]["annotations"]["platform.example/keep"],
        "true"
    );
}

#[tokio::test]
async fn inventory_uid_replacement_or_revision_race_never_writes() {
    let plan = plan();
    let policy = policy(&plan);
    let mut activation = activation(&plan, &policy);
    activation.status = ActivationStatus::Draining;
    for (uid, rv) in [("foreign-uid", "42"), ("inventory-uid", "43")] {
        let previous = inventory(&activation);
        let mut next = previous.clone();
        next.drained = true;
        let (client, calls) = wire(vec![
            object("activation", &activation, "activation-uid", "20"),
            object("inventory", &previous, uid, rv),
        ]);
        let authority =
            RuntimeAuthority::new(client, "control", activation.binding.clone()).unwrap();
        assert!(
            authority
                .publish_drained_inventory(
                    &Versioned {
                        value: previous,
                        uid: "inventory-uid".into(),
                        resource_version: "42".into()
                    },
                    next
                )
                .await
                .is_err()
        );
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .all(|(method, _, _)| method == "GET")
        );
    }
}

#[tokio::test]
async fn malformed_oversized_and_duplicate_control_records_fail_closed() {
    for value in [json!({"x":1}), json!("x".repeat(MAX_STATE_BYTES + 1))] {
        let mut record = object("registry", &value, REGISTRY_UID, "42");
        if value.is_object() {
            record["data"]["state.json"] = json!(r#"{"x":1,"x":2}"#);
        }
        let (client, _) = wire(vec![record]);
        assert!(
            platform_client(client, "control", "workload", "cluster-a")
                .unwrap()
                .load_registry()
                .await
                .is_err()
        );
    }
}

struct ReservationFixture {
    registry: NamespaceRegistry,
    policy: PolicyState,
    request: deploy::DeploymentRequest,
}

fn reservation_fixture() -> std::result::Result<ReservationFixture, Box<dyn std::error::Error>> {
    let compiled = plan();
    let policy = policy(&compiled);
    let history = deploy::GenerationHistory {
        domain_id: "example".into(),
        cluster_binding: "cluster-a".into(),
        revision: "42".into(),
        active_allocation: None,
    };
    let registry = hydrated_registry(
        1,
        vec![],
        BTreeMap::from([("example".into(), history.clone())]),
        BTreeMap::new(),
        BTreeMap::new(),
    )?;
    let request = deploy::DeploymentRequest {
        transition_id: TRANSITION_A.into(),
        previous: None,
        next: deploy::GenerationPlan {
            generation_id: GENERATION_A.into(),
            artifact_digest: format!("sha256:{}", "a".repeat(64)),
            compiled,
        },
        runtime_ceiling: policy.runtime_ceiling.clone(),
        cleanup_ceiling: policy.cleanup_ceiling.clone(),
        registry: deploy::RegistrySnapshot {
            registry_uid: REGISTRY_UID.into(),
            next_sequence: 1,
            revision: "42".into(),
            claims: vec![],
            generation_history: history,
        },
        observations: vec![],
        ownership_intents: vec![],
    };
    Ok(ReservationFixture {
        registry,
        policy,
        request,
    })
}

#[tokio::test]
async fn namespace_claim_reservation_is_one_conditional_update_and_rejects_stale_snapshot()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let ReservationFixture {
        registry,
        policy,
        request,
    } = reservation_fixture()?;
    let registry_response = registry_object(&registry, REGISTRY_UID, "42")?;
    let (client, calls) = wire(vec![
        registry_response.clone(),
        object("policy", &policy, "policy-uid", "40"),
        registry_response,
        json!("echo-put"),
    ]);
    let platform = platform_client(client, "control", "workload", "cluster-a")?;
    let changed = reserve_test_transition(&platform, &request, "ci_job_1").await?;
    assert_eq!(changed.value.claims.len(), 1);
    assert_eq!(changed.value.claims[0].state, ClaimState::Reserved);
    assert_eq!(changed.value.next_sequence, 2);
    assert_eq!(
        changed.value.transition(TRANSITION_A)?.record.executor_id,
        "ci_job_1"
    );
    assert!(
        changed.value.histories["example"]
            .active_allocation
            .is_none()
    );
    assert_eq!(
        changed.value.transitions[TRANSITION_A].record.phase,
        deploy::Phase::Planned
    );
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _, _)| m == "PUT")
            .count(),
        1
    );
    let mut stale = request.clone();
    stale.registry.revision = "41".into();
    let (client, calls) = wire(vec![registry_object(&registry, REGISTRY_UID, "42")?]);
    let platform = platform_client(client, "control", "workload", "cluster-a").unwrap();
    assert!(
        reserve_test_transition(&platform, &stale, "executor-b")
            .await
            .is_err()
    );
    assert_eq!(calls.lock().unwrap().len(), 2);

    let registry_response = registry_object(&registry, REGISTRY_UID, "42")?;
    let (client, calls) = wire(vec![
        registry_response.clone(),
        object("policy", &policy, "policy-uid", "40"),
        registry_response.clone(),
    ]);
    let lying_store = LyingClaimStore {
        snapshot: registry.claim_snapshot()?,
    };
    let platform = PlatformClient::new(client, "control", "workload", "cluster-a")?
        .with_claim_store(Arc::new(lying_store));
    assert!(
        reserve_test_transition(&platform, &request, "executor-b")
            .await
            .is_err()
    );
    assert!(
        calls
            .lock()
            .map_err(|_| "request lock poisoned")?
            .iter()
            .all(|(method, _, _)| method == "GET")
    );

    let (client, calls) = wire(vec![
        registry_response.clone(),
        object("policy", &policy, "policy-uid", "40"),
        registry_response,
        json!("echo-mutated-registry-put"),
    ]);
    let platform = platform_client(client, "control", "workload", "cluster-a")?;
    assert!(
        reserve_test_transition(&platform, &request, "executor-b")
            .await
            .is_err()
    );
    assert_eq!(
        calls
            .lock()
            .map_err(|_| "request lock poisoned")?
            .iter()
            .filter(|(method, _, _)| method == "PUT")
            .count(),
        1
    );
    Ok(())
}

fn expected_writers() -> ExpectedWriters {
    ExpectedWriters {
        deployment_name: "writer".into(),
        deployment_uid: "deployment-uid".into(),
        service_account_name: "writer".into(),
        pods: vec![ExpectedPod {
            name: "writer-pod".into(),
            uid: "pod-uid".into(),
            node_name: "node".into(),
            node_uid: "node-uid".into(),
            containers: CapturedContainers {
                regular: vec![CapturedContainer {
                    name: "controller".into(),
                    container_id: "containerd://one".into(),
                }],
                init: vec![],
                ephemeral: vec![],
            },
        }],
    }
}
fn stopped_deployment() -> Value {
    json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"writer","uid":"deployment-uid","resourceVersion":"80","generation":2},"spec":{"replicas":0,"selector":{"matchLabels":{"writer":"true"}},"template":{"metadata":{"finalizers":[TERMINATION_FINALIZER]},"spec":{"containers":[{"name":"controller"}]}}},"status":{"replicas":0,"observedGeneration":2}})
}
fn terminal_pod() -> Value {
    json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":"writer-pod","uid":"pod-uid","resourceVersion":"79","finalizers":[TERMINATION_FINALIZER]},"spec":{"serviceAccountName":"writer","nodeName":"node","containers":[{"name":"controller"}]},"status":{"phase":"Failed","containerStatuses":[{"name":"controller","image":"controller","imageID":"sha256:controller","ready":false,"restartCount":0,"state":{"terminated":{"exitCode":0,"finishedAt":"2026-09-05T00:00:00Z","containerID":"containerd://one"}}}]}})
}
fn ready_node() -> Value {
    json!({"apiVersion":"v1","kind":"Node","metadata":{"name":"node","uid":"node-uid"},"status":{"conditions":[{"type":"Ready","status":"True"}]}})
}

#[tokio::test]
async fn termination_requires_observed_zero_and_known_terminal_containers() {
    let deployment = stopped_deployment();
    let pod = terminal_pod();
    let (client, calls) = wire(vec![
        deployment.clone(),
        json!({"apiVersion":"v1","kind":"PodList","metadata":{"resourceVersion":"81"},"items":[pod.clone()]}),
        ready_node(),
        deployment.clone(),
    ]);
    let platform = platform_client(client, "control", "workload", "cluster-a").unwrap();
    platform
        .observe_writer_termination(&expected_writers())
        .await
        .unwrap();
    assert!(calls.lock().unwrap().iter().all(|c| c.0 == "GET"));
    for field in ["observedGeneration"] {
        let mut invalid_deployment = deployment.clone();
        invalid_deployment["status"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        let (client, calls) = wire(vec![invalid_deployment]);
        assert!(
            platform_client(client, "control", "workload", "cluster-a")
                .unwrap()
                .observe_writer_termination(&expected_writers())
                .await
                .is_err()
        );
        assert_eq!(calls.lock().unwrap().len(), 1);
    }
    let mut omitted_zero = deployment.clone();
    omitted_zero["status"]
        .as_object_mut()
        .unwrap()
        .remove("replicas");
    let (client, _) = wire(vec![
        omitted_zero.clone(),
        json!({"apiVersion":"v1","kind":"PodList","items":[pod.clone()]}),
        ready_node(),
        omitted_zero,
    ]);
    platform_client(client, "control", "workload", "cluster-a")
        .unwrap()
        .observe_writer_termination(&expected_writers())
        .await
        .unwrap();
    let mut unknown = pod;
    unknown["status"]["containerStatuses"][0]["state"]["terminated"]["reason"] =
        json!("ContainerStatusUnknown");
    let (client, _) = wire(vec![
        deployment,
        json!({"apiVersion":"v1","kind":"PodList","items":[unknown]}),
        ready_node(),
    ]);
    assert!(
        platform_client(client, "control", "workload", "cluster-a")
            .unwrap()
            .observe_writer_termination(&expected_writers())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn missing_pod_or_changed_node_never_proves_termination() {
    let (client, calls) = wire(vec![
        stopped_deployment(),
        json!({"apiVersion":"v1","kind":"PodList","items":[]}),
    ]);
    assert!(
        platform_client(client, "control", "workload", "cluster-a")
            .unwrap()
            .observe_writer_termination(&expected_writers())
            .await
            .is_err()
    );
    assert_eq!(calls.lock().unwrap().len(), 2);
    let mut node = ready_node();
    node["metadata"]["uid"] = json!("replacement-node");
    let (client, calls) = wire(vec![
        stopped_deployment(),
        json!({"apiVersion":"v1","kind":"PodList","items":[terminal_pod()]}),
        node,
    ]);
    assert!(
        platform_client(client, "control", "workload", "cluster-a")
            .unwrap()
            .observe_writer_termination(&expected_writers())
            .await
            .is_err()
    );
    assert_eq!(calls.lock().unwrap().len(), 3);
}

#[cfg(feature = "deployer")]
type StagedInitialFixture = (
    LifecycleRegistry,
    ActivationState,
    PolicyState,
    Value,
    Value,
);

#[cfg(feature = "deployer")]
fn staged_initial_fixture() -> std::result::Result<StagedInitialFixture, Box<dyn std::error::Error>>
{
    use deploy::{
        DeploymentRequest, Event, GenerationHistory, GenerationPlan, RegistrySnapshot, advance,
        begin, plan_transition,
    };
    let compiled = plan();
    let policy = policy(&compiled);
    let trust = json!({"platform":"approved"});
    let mut activation = activation(&compiled, &policy);
    activation.status = ActivationStatus::Inactive;
    let history = GenerationHistory {
        domain_id: "example".into(),
        cluster_binding: "cluster-a".into(),
        revision: "42".into(),
        active_allocation: None,
    };
    let request = DeploymentRequest {
        transition_id: TRANSITION_A.into(),
        previous: None,
        next: GenerationPlan {
            generation_id: GENERATION_A.into(),
            artifact_digest: activation.binding.build_digest.clone(),
            compiled,
        },
        runtime_ceiling: policy.runtime_ceiling.clone(),
        cleanup_ceiling: policy.cleanup_ceiling.clone(),
        registry: RegistrySnapshot {
            registry_uid: REGISTRY_UID.into(),
            next_sequence: 1,
            revision: "42".into(),
            claims: vec![],
            generation_history: history.clone(),
        },
        observations: vec![],
        ownership_intents: vec![],
    };
    let plan = plan_transition(&request).unwrap();
    activation
        .binding
        .service_account_name
        .clone_from(&plan.generation_reservation.service_account_name);
    let mut record = begin(&plan, "executor-a").unwrap();
    record = advance(
        &plan,
        &record,
        0,
        "executor-a",
        Event::Staged {
            event_id: "stage-0".into(),
            generation_id: GENERATION_A.into(),
            service_account_name: plan.generation_reservation.service_account_name.clone(),
            registry_cas_receipt_digest: digest(&"real fixture stage").unwrap(),
            verification_epoch: 0,
        },
    )
    .unwrap();
    let deployment = json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":activation.binding.service_account_name,"namespace":"control","uid":"controller-uid","resourceVersion":"50"},"spec":{"replicas":0}});
    let inputs = transition_inputs(&plan, &policy)?;
    let plans = BTreeMap::from([(TRANSITION_A.into(), plan.clone())]);
    let entry = staged_initial_transition(inputs, record, &activation, &trust, &plan)?;
    let mut claim = activation.approved_claims[0].claim.clone();
    claim.state = ClaimState::Reserved;
    let registry = hydrated_registry(
        2,
        vec![claim],
        [("example".into(), history)].into(),
        BTreeMap::new(),
        [(TRANSITION_A.into(), entry)].into(),
    )?;
    let mut registry = LifecycleRegistry { registry, plans };
    registry.admit(TRANSITION_A)?;
    Ok((registry, activation, policy, trust, deployment))
}

#[cfg(feature = "deployer")]
fn staged_initial_transition(
    inputs: TransitionInputs,
    record: deploy::TransitionRecord,
    activation: &ActivationState,
    trust: &Value,
    plan: &deploy::TransitionPlan,
) -> std::result::Result<StoredTransition, Box<dyn std::error::Error>> {
    let deployment_key = format!(
        "apps/v1/Deployment/control/{}",
        activation.binding.service_account_name
    );
    Ok(StoredTransition {
        next_review_metadata: fixture_review_store().seed(crate::project_review_metadata(plan)?)?,
        previous_review_metadata: None,
        capacity: CapacityAdmission::projection_placeholder(
            crate::completion_envelope::initial_profile(),
        ),
        inputs,
        record,
        staged_identity_uids: [
            (deployment_key.clone(), "controller-uid".into()),
            (
                "v1/ConfigMap/control/reakt-activation-example".into(),
                "activation-uid".into(),
            ),
        ]
        .into(),
        staged_content_digests: [(deployment_key, digest(&json!({"spec":{"replicas":0}}))?)].into(),
        runtime_binding: Some(activation.binding.clone()),
        staged_trust_policy_digest: Some(digest(trust)?),
        staged_provenance_digest: Some(digest(&"verified provenance")?),
        request_drain_qualification: Some(RequestDrainQualification::UnqualifiedDirectApi),
        proxy_identity: None,
        upgrade_drain: None,
        previous_writer_checkpoint: None,
    })
}

#[cfg(feature = "deployer")]
fn stateful_wire(objects: Vec<Value>) -> (Client, Calls) {
    stateful_wire_with_drift(objects, false)
}

#[cfg(feature = "deployer")]
fn stateful_wire_with_drift(
    objects: Vec<Value>,
    drift_after_deployment_read: bool,
) -> (Client, Calls) {
    let objects = objects
        .into_iter()
        .chain(std::iter::once(registry_authority()));
    let store: Arc<Mutex<BTreeMap<String, Value>>> = Arc::new(Mutex::new(
        objects
            .into_iter()
            .map(|o| {
                let resource = match o["kind"].as_str().unwrap() {
                    "Deployment" => "deployments",
                    "ServiceAccount" => "serviceaccounts",
                    "Pod" => "pods",
                    "Node" => "nodes",
                    "Secret" => "secrets",
                    "Service" => "services",
                    "Role" => "roles",
                    "RoleBinding" => "rolebindings",
                    _ => "configmaps",
                };
                (
                    format!(
                        "{}/{resource}/{}",
                        o["metadata"]["namespace"]
                            .as_str()
                            .map_or("v1", |namespace| namespace),
                        o["metadata"]["name"].as_str().unwrap()
                    ),
                    o,
                )
            })
            .collect(),
    ));
    let calls: Calls = Arc::default();
    let capture = calls.clone();
    let service = service_fn(move |request: Request<Body>| {
        let store = store.clone();
        let capture = capture.clone();
        async move {
            let (parts, body) = request.into_parts();
            let bytes = body.collect().await.unwrap().to_bytes();
            let value: Value = if bytes.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&bytes).unwrap()
            };
            capture.lock().unwrap().push((
                parts.method.to_string(),
                parts.uri.to_string(),
                value.clone(),
            ));
            let segments: Vec<_> = parts.uri.path().split('/').collect();
            let key = segments[segments.len() - 3..].join("/");
            let mut store = store.lock().unwrap();
            let current = store.get(&key).unwrap().clone();
            if drift_after_deployment_read
                && parts.method == http::Method::GET
                && parts.uri.path().contains("/deployments/")
            {
                for object in store.values_mut().filter(|o| o["kind"] == "Role") {
                    object["rules"] = json!([{"apiGroups":["*"],"resources":["*"],"verbs":["*"]}]);
                }
            }
            let reply = if parts.method == http::Method::PUT {
                assert_eq!(value["metadata"]["uid"], current["metadata"]["uid"]);
                assert_eq!(
                    value["metadata"]["resourceVersion"],
                    current["metadata"]["resourceVersion"]
                );
                let mut next = value;
                let rv = current["metadata"]["resourceVersion"]
                    .as_str()
                    .unwrap()
                    .parse::<u64>()
                    .unwrap()
                    + 1;
                next["metadata"]["resourceVersion"] = json!(rv.to_string());
                store.insert(key, next.clone());
                next
            } else {
                assert_eq!(parts.method, http::Method::GET);
                current
            };
            Ok::<_, Infallible>(
                Response::builder()
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&reply).unwrap()))
                    .unwrap(),
            )
        }
    });
    (Client::new(service, "control"), calls)
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn initial_activation_starts_only_after_protected_authority_and_resumes_committed()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (mut registry, activation, policy, trust, deployment) = staged_initial_fixture()?;
    let mut objects = initial_support_fixture(&mut registry)?;
    objects.extend(qualified_writer::qualify(&mut registry, TRANSITION_A)?);
    objects.extend(vec![
        registry_object(&registry, REGISTRY_UID, "42")?,
        object(
            "reakt-activation-example",
            &activation,
            "activation-uid",
            "45",
        ),
        object("reakt-policy-example", &policy, "policy-uid", "44"),
        object("reakt-trust-example", &trust, "trust-uid", "43"),
        deployment,
    ]);
    let (client, calls) = stateful_wire(objects);
    let platform = registry
        .bind(client, "control", "workload", "cluster-a", TRANSITION_A)
        .unwrap();
    let committed = Box::pin(platform.activate_initial(TRANSITION_A, "executor-a"))
        .await
        .unwrap();
    assert_eq!(
        committed.value.transitions[TRANSITION_A].record.phase,
        deploy::Phase::Committed
    );
    assert_eq!(committed.value.claims[0].state, ClaimState::Active);
    let first = calls.lock().unwrap().clone();
    for (_, _, payload) in first.iter().filter(|(method, path, _)| {
        method == "PUT" && path.ends_with("/configmaps/reakt-registry-workload")
    }) {
        let encoded = payload["data"]["state.json"]
            .as_str()
            .ok_or("registry state missing")?;
        let state = hydrate_registry(encoded)?;
        let record = state.transition(TRANSITION_A)?;
        let history = state
            .histories
            .get("example")
            .ok_or("domain history missing")?;
        if record.record.phase == deploy::Phase::Committed {
            let checkpoint = state
                .active_writer_checkpoints
                .get("example")
                .ok_or("committed checkpoint missing")?;
            assert_eq!(
                history.active_allocation.as_ref(),
                Some(&checkpoint.allocation)
            );
            assert_eq!(checkpoint.allocation, *record.allocation());
        } else {
            assert!(history.active_allocation.is_none());
            assert!(state.active_writer_checkpoints.is_empty());
        }
    }

    let activate = first
        .iter()
        .position(|c| c.0 == "PUT" && c.1.contains("/reakt-activation-example"))
        .unwrap();
    let start = first
        .iter()
        .position(|c| c.0 == "PUT" && c.1.contains("/deployments/"))
        .unwrap();
    assert!(activate < start);
    assert_eq!(first[start].2["spec"]["replicas"], json!(1));
    Box::pin(platform.activate_initial(TRANSITION_A, "executor-a"))
        .await
        .unwrap();
    let subsequent = calls.lock().unwrap();
    assert!(subsequent[first.len()..].iter().all(|c| c.0 == "GET"));
    Ok(())
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn initial_activation_rejects_changed_trust_without_writes()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (mut registry, _, _, trust, _) = staged_initial_fixture()?;
    qualified_writer::qualify(&mut registry, TRANSITION_A)?;
    let (client, calls) = wire(vec![
        registry_object(&registry, REGISTRY_UID, "42")?,
        object(
            "reakt-trust-example",
            &json!({"platform":"changed"}),
            "trust-uid",
            "43",
        ),
    ]);
    let platform = registry
        .bind(client, "control", "workload", "cluster-a", TRANSITION_A)?
        .with_test_trust(&digest(&trust)?)?;
    assert!(
        Box::pin(platform.activate_initial(TRANSITION_A, "executor-a"))
            .await
            .is_err()
    );
    let calls = calls.lock().map_err(|_| "calls lock poisoned")?;
    assert!(calls.iter().all(|call| call.0 == "GET"));
    assert!(
        calls
            .iter()
            .any(|call| call.1.ends_with("/configmaps/reakt-trust-example"))
    );
    Ok(())
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn runtime_identity_stage_rejects_changed_bound_trust_before_writes()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (registry, _, _, trust, _) = staged_initial_fixture()?;
    let (client, calls) = wire(vec![
        registry_object(&registry, REGISTRY_UID, "42")?,
        object(
            "reakt-trust-example",
            &json!({"platform":"changed"}),
            "trust-uid",
            "43",
        ),
    ]);
    let platform = registry
        .bind(client, "control", "workload", "cluster-a", TRANSITION_A)?
        .with_test_trust(&digest(&trust)?)?;
    assert!(
        platform
            .stage_runtime_identity(TRANSITION_A, "executor-a")
            .await
            .is_err()
    );
    let calls = calls.lock().map_err(|_| "calls lock poisoned")?;
    assert!(calls.iter().all(|call| call.0 == "GET"));
    assert!(
        calls
            .iter()
            .any(|call| call.1.ends_with("/configmaps/reakt-trust-example"))
    );
    Ok(())
}

#[cfg(feature = "deployer")]
fn staged_upgrade_fixture() -> std::result::Result<LifecycleRegistry, Box<dyn std::error::Error>> {
    use deploy::{Event, Phase, Quiescence, RegistrySnapshot, advance, begin, plan_transition};
    let (mut registry, _, policy, _, _) = staged_initial_fixture()?;
    let previous_plan = registry.plan(TRANSITION_A)?.clone();
    let previous = registry
        .transitions
        .get_mut(TRANSITION_A)
        .ok_or("initial transition missing")?;
    for event in [
        Event::Quiesced {
            event_id: "initial-quiesce".into(),
            evidence: Quiescence::InitialInstallation {
                verification_epoch: 0,
            },
        },
        Event::Activated {
            event_id: "initial-active".into(),
            generation_id: GENERATION_A.into(),
            activation_receipt_digest: digest(&"active")?,
            verification_epoch: 0,
        },
        Event::Cleaning {
            event_id: "initial-cleaning".into(),
        },
        Event::Committed {
            event_id: "initial-commit".into(),
            cleanup_receipts: vec![],
            obsolete_identity_retirement_digest: digest(&"no predecessor")?,
        },
    ] {
        previous.record = advance(
            &previous_plan,
            &previous.record,
            previous.record.revision,
            "executor-a",
            event,
        )?;
    }
    assert_eq!(previous.record.phase, Phase::Committed);
    let previous = previous.clone();
    registry
        .histories
        .get_mut("example")
        .ok_or("domain history missing")?
        .active_allocation = Some(previous_plan.generation_reservation.clone());
    for claim in &mut registry.claims {
        claim.state = ClaimState::Active;
        claim
            .transition_id
            .clone_from(&previous_plan.request.transition_id);
    }
    let mut request = previous_plan.request.clone();
    request.transition_id = TRANSITION_B.into();
    request.previous = Some(deploy::PreviousGeneration::Metadata(Box::new(
        deploy::predecessor_metadata(
            &previous_plan.request.next,
            &previous_plan.generation_reservation,
            &registry.claims,
        )?,
    )));
    request.next.generation_id = GENERATION_B.into();
    request.registry = RegistrySnapshot {
        registry_uid: REGISTRY_UID.into(),
        next_sequence: 2,
        revision: "42".into(),
        claims: registry.claims.clone(),
        generation_history: registry
            .histories
            .get("example")
            .ok_or("domain history missing")?
            .clone(),
    };
    let plan = plan_transition(&request)?;
    let mut current = previous;
    current.record = begin(&plan, "executor-b")?;
    current.record = advance(
        &plan,
        &current.record,
        0,
        "executor-b",
        Event::Staged {
            event_id: "stage-b".into(),
            generation_id: GENERATION_B.into(),
            service_account_name: plan.generation_reservation.service_account_name.clone(),
            registry_cas_receipt_digest: digest(&"new source staging")?,
            verification_epoch: 0,
        },
    )?;
    current.previous_review_metadata = Some(current.next_review_metadata.clone());
    current.next_review_metadata =
        fixture_review_store().seed(crate::project_review_metadata(&plan)?)?;
    current.inputs = transition_inputs(&plan, &policy)?;
    let current = retarget_staged_identity(current, &previous_plan, &plan)?;
    registry.transitions.insert(TRANSITION_B.into(), current);
    registry.plans.insert(TRANSITION_B.into(), plan);
    registry.next_sequence = 3;
    // This intermediate fixture has no qualified predecessor checkpoint.
    // qualified_upgrade_fixture completes authority before admitting the upgrade.
    Ok(registry)
}

#[cfg(feature = "deployer")]
fn retarget_staged_identity(
    mut current: StoredTransition,
    previous_plan: &deploy::TransitionPlan,
    plan: &deploy::TransitionPlan,
) -> std::result::Result<StoredTransition, Box<dyn std::error::Error>> {
    current
        .runtime_binding
        .as_mut()
        .ok_or("runtime binding missing")?
        .generation_id = GENERATION_B.into();
    let old_account = previous_plan
        .generation_reservation
        .service_account_name
        .as_str();
    let new_account = plan.generation_reservation.service_account_name.as_str();
    current
        .runtime_binding
        .as_mut()
        .ok_or("runtime binding missing")?
        .service_account_name = new_account.into();
    current.staged_identity_uids = current
        .staged_identity_uids
        .into_iter()
        .map(|(key, uid)| (key.replace(old_account, new_account), uid))
        .collect();
    current.staged_content_digests = current
        .staged_content_digests
        .into_iter()
        .map(|(key, value)| (key.replace(old_account, new_account), value))
        .collect();
    Ok(current)
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn predecessor_metadata_requires_exact_protected_retirement_authority_before_reservation()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    for alteration in [
        "extra-namespace",
        "missing-namespace",
        "wrong-transition",
        "wrong-scope",
        "wrong-control",
        "missing-checkpoint",
        "changed-writer",
    ] {
        let mut registry = with_active_checkpoint(qualified_upgrade_fixture()?)?;
        let mut request = registry.plan(TRANSITION_B)?.request.clone();
        registry
            .transitions
            .remove(TRANSITION_B)
            .ok_or("upgrade transition missing")?;
        registry.next_sequence = request.registry.next_sequence;
        let Some(deploy::PreviousGeneration::Metadata(previous)) = request.previous.as_mut() else {
            return Err("fixture must use predecessor metadata".into());
        };
        match alteration {
            "extra-namespace" => previous
                .retirement_role_binding_namespaces
                .push("unapproved".into()),
            "missing-namespace" => previous.retirement_role_binding_namespaces.clear(),
            "wrong-transition" => previous.allocation.transition_id = "unrelated-transition".into(),
            "wrong-scope" => previous.scope.namespace = "unrelated".into(),
            "wrong-control" => previous.control_namespace = "unrelated".into(),
            "missing-checkpoint" => {
                registry.active_writer_checkpoints.clear();
            }
            "changed-writer" => {
                registry
                    .active_writer_checkpoints
                    .get_mut("example")
                    .ok_or("checkpoint missing")?
                    .runtime_binding
                    .generation_id = "foreign".into();
            }
            _ => return Err("unknown fixture alteration".into()),
        }
        let (client, calls) = wire(vec![registry_object(&registry, REGISTRY_UID, "42")?]);
        let platform = platform_client(client, "control", "workload", "cluster-a")?;
        assert!(
            reserve_test_transition(&platform, &request, "executor-b")
                .await
                .is_err(),
            "{alteration}"
        );
        let calls = calls.lock().map_err(|_| "request lock poisoned")?;
        assert!(calls.iter().all(|call| call.0 == "GET"));
        assert!(calls.len() <= 2, "{alteration} must reject before policy");
    }
    Ok(())
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn authenticated_metadata_predecessor_reserves_without_embedding_old_plan()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut registry = with_active_checkpoint(qualified_upgrade_fixture()?)?;
    let topology = capacity_topology::admission_reads(&mut registry)?;
    registry.plans.remove(TRANSITION_A);
    let request = registry.plan(TRANSITION_B)?.request.clone();
    registry
        .transitions
        .remove(TRANSITION_B)
        .ok_or("upgrade transition missing")?;
    registry.next_sequence = request.registry.next_sequence;
    let policy = policy(&request.next.compiled);
    let mut responses = vec![
        registry_object(&registry, REGISTRY_UID, "42")?,
        object("policy", &policy, "policy-uid", "40"),
    ];
    responses.extend(topology);
    responses.extend([
        registry_object(&registry, REGISTRY_UID, "42")?,
        json!("echo-put"),
    ]);
    let (client, calls) = wire(responses);
    let platform = platform_client(client, "control", "workload", "cluster-a")?;
    let reserved = reserve_test_transition(&platform, &request, "executor-b").await?;
    assert!(
        reserved
            .value
            .transition(TRANSITION_B)?
            .previous_writer_checkpoint
            .is_some()
    );
    let previous = serde_json::to_value(&request.previous)?;
    assert!(previous.get("compiled").is_none());
    assert!(previous.get("inventory").is_some());
    let compact = serde_json::to_string(reserved.value.transition(TRANSITION_B)?)?;
    for forbidden in [
        "\"compiled\"",
        "\"desired\"",
        "\"source\"",
        "\"claims\"",
        "\"inventory\"",
        "\"permittedApply\"",
    ] {
        assert!(!compact.contains(forbidden), "{forbidden}");
    }
    assert_eq!(
        calls
            .lock()
            .map_err(|_| "request lock poisoned")?
            .iter()
            .filter(|call| call.0 == "PUT")
            .count(),
        1
    );
    Ok(())
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn unqualified_predecessor_and_forged_phase_never_write()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    use deploy::Phase;
    let registry = staged_upgrade_fixture()?;
    let (client, calls) = wire(vec![
        registry_object(&registry, REGISTRY_UID, "42")?,
        registry_object(&registry, REGISTRY_UID, "42")?,
        registry_object(&registry, REGISTRY_UID, "42")?,
        registry_object(&registry, REGISTRY_UID, "42")?,
        json!("echo-put"),
    ]);
    let platform = registry
        .bind(client, "control", "workload", "cluster-a", TRANSITION_B)
        .unwrap();
    assert!(
        platform
            .quiesce_previous(TRANSITION_B, "executor-b")
            .await
            .is_err()
    );
    assert_eq!(calls.lock().unwrap().len(), 2);
    assert!(calls.lock().unwrap().iter().all(|call| call.0 == "GET"));

    for phase in [Phase::Quiesced, Phase::Activated, Phase::Cleaning] {
        let mut forged = registry.clone();
        forged
            .transitions
            .get_mut(TRANSITION_B)
            .unwrap()
            .record
            .phase = phase;
        let (client, calls) = wire(vec![registry_object(&forged, REGISTRY_UID, "42")?]);
        let platform = forged
            .bind(client, "control", "workload", "cluster-a", TRANSITION_B)
            .unwrap();
        assert!(
            platform
                .activate_upgrade(TRANSITION_B, "executor-b")
                .await
                .is_err()
        );
        assert_eq!(calls.lock().unwrap().len(), 2);
        assert_eq!(calls.lock().unwrap()[0].0, "GET");
    }
    Ok(())
}

#[cfg(feature = "deployer")]
fn initial_support_fixture(
    registry: &mut LifecycleRegistry,
) -> std::result::Result<Vec<Value>, Box<dyn std::error::Error>> {
    let plan = registry.plan(TRANSITION_A)?.clone();
    let stored = registry
        .transitions
        .get_mut(TRANSITION_A)
        .ok_or("initial transition missing")?;
    let name = stored.allocation().service_account_name.clone();
    let annotations = json!({"reakt.dev/transition":TRANSITION_A,"reakt.dev/generation":GENERATION_A,"reakt.dev/plan-digest":stored.plan_digest()});
    let mut objects = vec![
        json!({"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":name,"namespace":"control","annotations":annotations},"automountServiceAccountToken":false}),
    ];
    let mut rules: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for capability in &plan.request.next.compiled.capabilities {
        let resource = capability.subresource.as_ref().map_or_else(
            || capability.resource.clone(),
            |s| format!("{}/{s}", capability.resource),
        );
        let mut rule = json!({"apiGroups":[capability.api_group],"resources":[resource],"verbs":[capability.verb]});
        if !capability.names.is_empty() {
            rule["resourceNames"] = json!(capability.names);
        }
        rules
            .entry(capability.namespace.clone())
            .or_default()
            .push(rule);
    }
    for (namespace, rules) in rules {
        objects.push(json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"Role","metadata":{"name":name,"namespace":namespace,"annotations":annotations},"rules":rules}));
        objects.push(json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"RoleBinding","metadata":{"name":name,"namespace":namespace,"annotations":annotations},"roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"Role","name":name},"subjects":[{"kind":"ServiceAccount","name":name,"namespace":"control"}]}));
    }
    objects.push(json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":format!("{name}-config"),"namespace":"control"},"immutable":true,"data":{"config.json":"fixture immutable startup"}}));
    for (i, object) in objects.iter_mut().enumerate() {
        object["metadata"]["uid"] = json!(format!("support-{i}"));
        object["metadata"]["resourceVersion"] = json!("40");
        let key = format!(
            "{}/{}/{}/{}",
            object["apiVersion"].as_str().unwrap(),
            object["kind"].as_str().unwrap(),
            object["metadata"]["namespace"].as_str().unwrap(),
            object["metadata"]["name"].as_str().unwrap()
        );
        stored
            .staged_identity_uids
            .insert(key.clone(), format!("support-{i}"));
        if object["kind"] == "ConfigMap" {
            stored.staged_content_digests.insert(
                key,
                digest(&json!({"immutable":true,"data":object["data"]})).unwrap(),
            );
        }
    }
    Ok(objects)
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn broadened_staged_role_and_recreated_config_block_before_activation()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    for changed_kind in ["Role", "ConfigMap"] {
        let (mut registry, activation, policy, trust, deployment) = staged_initial_fixture()?;
        let mut objects = initial_support_fixture(&mut registry)?;
        objects.extend(qualified_writer::qualify(&mut registry, TRANSITION_A)?);
        let changed = objects
            .iter_mut()
            .find(|o| o["kind"] == changed_kind)
            .unwrap();
        if changed_kind == "Role" {
            changed["rules"] = json!([{"apiGroups":["*"],"resources":["*"],"verbs":["*"]}]);
        } else {
            changed["metadata"]["uid"] = json!("replaced-config-uid");
        }
        objects.extend(vec![
            registry_object(&registry, REGISTRY_UID, "42")?,
            object("reakt-policy-example", &policy, "policy-uid", "44"),
            object("reakt-trust-example", &trust, "trust-uid", "43"),
            object(
                "reakt-activation-example",
                &activation,
                "activation-uid",
                "45",
            ),
            deployment,
        ]);
        let (client, calls) = stateful_wire(objects);
        let platform = registry
            .bind(client, "control", "workload", "cluster-a", TRANSITION_A)
            .unwrap();
        assert!(
            Box::pin(platform.activate_initial(TRANSITION_A, "executor-a"))
                .await
                .is_err()
        );
        assert!(calls.lock().unwrap().iter().all(|c| c.0 == "GET"));
    }
    Ok(())
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn crash_after_start_resume_rechecks_authority_before_replica_one_fast_path()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    use deploy::{Event, Quiescence, advance};
    let (mut registry, activation, policy, trust, mut deployment) = staged_initial_fixture()?;
    let plan = registry.plan(TRANSITION_A)?.clone();
    let entry = registry.transitions.get_mut(TRANSITION_A).unwrap();
    for event in [
        Event::Quiesced {
            event_id: "q".into(),
            evidence: Quiescence::InitialInstallation {
                verification_epoch: 0,
            },
        },
        Event::Activated {
            event_id: "a".into(),
            generation_id: GENERATION_A.into(),
            activation_receipt_digest: digest(&"real activation").unwrap(),
            verification_epoch: 0,
        },
    ] {
        entry.record = advance(
            &plan,
            &entry.record,
            entry.record.revision,
            "executor-a",
            event,
        )
        .unwrap();
    }
    deployment["spec"]["replicas"] = json!(1);
    let mut objects = initial_support_fixture(&mut registry)?;
    objects.extend(qualified_writer::qualify(&mut registry, TRANSITION_A)?);
    objects.extend(vec![
        registry_object(&registry, REGISTRY_UID, "42")?,
        object("reakt-policy-example", &policy, "policy-uid", "44"),
        object("reakt-trust-example", &trust, "trust-uid", "43"),
        object(
            "reakt-activation-example",
            &activation,
            "activation-uid",
            "45",
        ),
        deployment,
    ]);
    let (client, calls) = stateful_wire_with_drift(objects, true);
    let platform = registry
        .bind(client, "control", "workload", "cluster-a", TRANSITION_A)
        .unwrap();
    assert!(
        Box::pin(platform.activate_initial(TRANSITION_A, "executor-a"))
            .await
            .is_err()
    );
    assert!(calls.lock().unwrap().iter().all(|c| c.0 == "GET"));
    let roles = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.1.contains("/roles/"))
        .count();
    assert!(
        roles >= 3,
        "resume must recheck after initially valid two-namespace role reads"
    );
    Ok(())
}

fn adoption_request() -> std::result::Result<
    (NamespaceRegistry, PolicyState, deploy::DeploymentRequest),
    Box<dyn std::error::Error>,
> {
    let compiled = plan();
    let policy = policy(&compiled);
    let history = deploy::GenerationHistory {
        domain_id: "example".into(),
        cluster_binding: "cluster-a".into(),
        revision: "42".into(),
        active_allocation: None,
    };
    let registry = hydrated_registry(
        1,
        vec![],
        BTreeMap::from([("example".into(), history.clone())]),
        BTreeMap::new(),
        BTreeMap::new(),
    )?;
    let next = deploy::GenerationPlan {
        generation_id: GENERATION_A.into(),
        artifact_digest: format!("sha256:{}", "a".repeat(64)),
        compiled,
    };
    let approval: deploy::OwnershipApproval = serde_json::from_value(json!({
        "schemaVersion":"reakt.dev/ownership-intent/v1", "transitionId":TRANSITION_A,
        "domainId":"example", "generationId":GENERATION_A, "releaseDigest":next.artifact_digest,
        "clusterBinding":"cluster-a", "controlNamespace":"control",
        "identity":{"apiGroup":"", "resource":"configmaps", "namespace":"workload", "name":"config"},
        "observedUid":"existing-uid", "observedResourceVersion":"17",
        "priorManagedFieldsDigest":digest(&Value::Null).unwrap(),
        "permittedApply":next.compiled.ir.resources[0].desired,
        "permittedRuntimeFields":[], "deletionPolicy":"orphan", "priorManagerConflicts":[],
        "action":{"kind":"adopt"}
    })).unwrap();
    let intent = deploy::OwnershipIntent {
        object_name: "approve-config".into(),
        object_uid: "approval-uid".into(),
        object_resource_version: "12".into(),
        approval: approval.clone(),
    };
    let request = deploy::DeploymentRequest {
        transition_id: TRANSITION_A.into(),
        previous: None,
        next,
        runtime_ceiling: policy.runtime_ceiling.clone(),
        cleanup_ceiling: policy.cleanup_ceiling.clone(),
        registry: deploy::RegistrySnapshot {
            registry_uid: REGISTRY_UID.into(),
            next_sequence: 1,
            revision: "42".into(),
            claims: vec![],
            generation_history: history,
        },
        observations: vec![],
        ownership_intents: vec![intent],
    };
    Ok((registry, policy, request))
}

#[tokio::test]
async fn adoption_reservation_rechecks_operator_approval_and_uses_non_force_uid_bound_dry_run()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (registry, policy, request) = adoption_request()?;
    let approval = &request.ownership_intents[0].approval;
    for corruption in [
        "none",
        "approval-uid",
        "object-uid",
        "object-version",
        "managed-fields",
        "dry-run-uid",
    ] {
        let registry_object = registry_object(&registry, REGISTRY_UID, "42")?;
        let mut approval_object = object("approve-config", &approval, "approval-uid", "12");
        approval_object["metadata"]["namespace"] = json!(OWNERSHIP_INTENT_NAMESPACE);
        let mut live = json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{
            "name":"config","namespace":"workload","uid":"existing-uid","resourceVersion":"17"
        },"data":{"value":"approved"}});
        let mut dry_run = live.clone();
        match corruption {
            "approval-uid" => approval_object["metadata"]["uid"] = json!("replacement"),
            "object-uid" => live["metadata"]["uid"] = json!("replacement"),
            "object-version" => live["metadata"]["resourceVersion"] = json!("18"),
            "managed-fields" => live["metadata"]["managedFields"] = json!([]),
            "dry-run-uid" => dry_run["metadata"]["uid"] = json!("replacement"),
            _ => {}
        }
        let (client, calls) = wire(vec![
            registry_object.clone(),
            object("policy", &policy, "policy-uid", "40"),
            approval_object,
            live,
            dry_run,
            registry_object,
            json!("echo-put"),
        ]);
        let platform = platform_client(client, "control", "workload", "cluster-a").unwrap();
        let result = reserve_test_transition(&platform, &request, "executor-a").await;
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls[3].1,
            "/api/v1/namespaces/reakt-platform-intents/configmaps/approve-config"
        );
        if corruption == "none" {
            let reserved = result.unwrap();
            assert_eq!(
                reserved.value.claims[0].uid.as_deref(),
                Some("existing-uid")
            );
            let patch = &calls[5];
            assert_eq!(patch.0, "PATCH");
            assert!(patch.1.contains("dryRun=All"));
            assert!(patch.1.contains("fieldValidation=Strict"));
            assert!(!patch.1.contains("force=true"));
            assert_eq!(patch.2["metadata"]["uid"], "existing-uid");
            assert_eq!(patch.2["metadata"]["resourceVersion"], "17");
            assert_eq!(calls.last().unwrap().0, "PUT");
        } else {
            assert!(result.is_err(), "{corruption}");
            assert!(calls.iter().all(|call| call.0 != "PUT"));
        }
    }
    Ok(())
}

#[cfg(feature = "deployer")]
fn committed_inventory_fixture()
-> std::result::Result<LifecycleRegistry, Box<dyn std::error::Error>> {
    use deploy::{Event, Quiescence, advance};
    let (mut registry, _, _, _, _) = staged_initial_fixture()?;
    let plan = registry.plan(TRANSITION_A)?.clone();
    let stored = registry.transitions.get_mut(TRANSITION_A).unwrap();
    for event in [
        Event::Quiesced {
            event_id: "quiesce".into(),
            evidence: Quiescence::InitialInstallation {
                verification_epoch: 0,
            },
        },
        Event::Activated {
            event_id: "activate".into(),
            generation_id: GENERATION_A.into(),
            activation_receipt_digest: digest(&"activation").unwrap(),
            verification_epoch: 0,
        },
        Event::Cleaning {
            event_id: "cleaning".into(),
        },
        Event::Committed {
            event_id: "commit".into(),
            cleanup_receipts: vec![],
            obsolete_identity_retirement_digest: digest(&"no previous identity").unwrap(),
        },
    ] {
        stored.record = advance(
            &plan,
            &stored.record,
            stored.record.revision,
            "executor-a",
            event,
        )
        .unwrap();
    }
    registry
        .histories
        .get_mut("example")
        .unwrap()
        .active_allocation = Some(plan.generation_reservation.clone());
    registry.claims[0].state = ClaimState::Active;
    registry.claims[0].uid = Some("observed-uid".into());
    initial_support_fixture(&mut registry)?;
    qualified_writer::qualify(&mut registry, TRANSITION_A)?;
    let checkpoint = fixture_writer_checkpoint(&registry)?;
    registry
        .active_writer_checkpoints
        .insert("example".into(), checkpoint);
    Ok(registry)
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn committed_inventory_persists_metadata_and_retries_after_reopen()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    use inventory::{InventoryStore, LibsqlInventory};
    let registry = committed_inventory_fixture()?;
    let policy = policy(&registry.plan(TRANSITION_A)?.request.next.compiled);
    let response = registry_object(&registry, REGISTRY_UID, "42")?;
    let (client, calls) = stateful_wire(vec![
        response,
        object("reakt-policy-example", &policy, "policy-uid", "44"),
    ]);
    let platform = registry
        .bind(client, "control", "workload", "cluster-a", TRANSITION_A)
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("inventory.db");
    let stored = {
        let store = LibsqlInventory::open_local(&path).await.unwrap();
        platform
            .publish_committed_inventory(&store, TRANSITION_A, "executor-a")
            .await
            .unwrap()
    };
    assert_eq!(
        stored.snapshot.resources[0].uid.as_deref(),
        Some("observed-uid")
    );
    let reopened = LibsqlInventory::open_local(&path).await.unwrap();
    assert_eq!(
        platform
            .publish_committed_inventory(&reopened, TRANSITION_A, "executor-a")
            .await
            .unwrap(),
        stored
    );
    let scope =
        deploy::generation_inventory(&registry.plan(TRANSITION_A)?.request.next, &registry.claims)
            .unwrap()
            .0;
    assert_eq!(reopened.load(&scope).await.unwrap(), Some(stored));
    let bytes = std::fs::read(path).unwrap();
    for private in [
        b"approved".as_slice(),
        b"program.ts",
        b"desired",
        b"planDigest",
    ] {
        assert!(!bytes.windows(private.len()).any(|window| window == private));
    }
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 18);
    assert!(calls.iter().all(|call| {
        call.0 == "GET"
            && (call.1.ends_with("/configmaps/reakt-registry-workload")
                || call.1.ends_with("/configmaps/reakt-policy-example")
                || call
                    .1
                    .ends_with("/configmaps/reakt-registry-authority.control.workload"))
    }));
    Ok(())
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn registry_races_do_not_report_inventory_completion()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    use inventory::{InventoryStore, LibsqlInventory};
    let registry = committed_inventory_fixture()?;
    let scope =
        deploy::generation_inventory(&registry.plan(TRANSITION_A)?.request.next, &registry.claims)
            .unwrap()
            .0;
    let stable = registry_object(&registry, REGISTRY_UID, "42")?;
    let changed = registry_object(&registry, REGISTRY_UID, "43")?;
    let policy = object(
        "reakt-policy-example",
        &policy(&registry.plan(TRANSITION_A)?.request.next.compiled),
        "policy-uid",
        "44",
    );
    for after_commit in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let store = LibsqlInventory::open_local(&directory.path().join("inventory.db"))
            .await
            .unwrap();
        let replies = if after_commit {
            vec![
                stable.clone(),
                policy.clone(),
                stable.clone(),
                policy.clone(),
                changed.clone(),
                policy.clone(),
            ]
        } else {
            vec![
                stable.clone(),
                policy.clone(),
                changed.clone(),
                policy.clone(),
            ]
        };
        let (client, _) = wire(replies);
        let platform = registry
            .bind(client, "control", "workload", "cluster-a", TRANSITION_A)
            .unwrap();
        assert!(
            platform
                .publish_committed_inventory(&store, TRANSITION_A, "executor-a")
                .await
                .unwrap_err()
                .to_string()
                .contains("registry changed")
        );
        assert_eq!(store.load(&scope).await.unwrap().is_some(), after_commit);
        let (client, _) = wire(vec![
            changed.clone(),
            policy.clone(),
            changed.clone(),
            policy.clone(),
            changed.clone(),
            policy.clone(),
        ]);
        let platform = registry
            .bind(client, "control", "workload", "cluster-a", TRANSITION_A)
            .unwrap();
        assert!(
            platform
                .publish_committed_inventory(&store, TRANSITION_A, "executor-a")
                .await
                .is_ok()
        );
    }
    Ok(())
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn inventory_refuses_forged_commits_wrong_claims_and_uid_regression()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    use inventory::{InventoryStore, LibsqlInventory};
    for corrupt in [
        "receipts",
        "claim",
        "history",
        "uid",
        "replacement",
        "head",
        "missing-resource",
        "policy",
        "api-type",
    ] {
        let mut registry = committed_inventory_fixture()?;
        let (scope, mut snapshot) = deploy::generation_inventory(
            &registry.plan(TRANSITION_A)?.request.next,
            &registry.claims,
        )?;
        if corrupt == "head" {
            snapshot.generation = "newer-generation".into();
        }
        match corrupt {
            "missing-resource" => snapshot.resources.clear(),
            "policy" => {
                snapshot
                    .resources
                    .first_mut()
                    .ok_or("missing fixture resource")?
                    .deletion_policy = compiler_core::DeletionPolicy::Delete;
            }
            "api-type" => {
                snapshot
                    .resources
                    .first_mut()
                    .ok_or("missing fixture resource")?
                    .kind = "Secret".into();
            }
            _ => {}
        }
        let directory = tempfile::tempdir()?;
        let store = LibsqlInventory::open_local(&directory.path().join("inventory.db")).await?;
        let published = store.publish(&scope, None, &snapshot).await?;
        let claim = registry.claims.first_mut().ok_or("missing fixture claim")?;
        match corrupt {
            "receipts" => registry
                .transitions
                .get_mut(TRANSITION_A)
                .ok_or("missing fixture transition")?
                .record
                .receipts
                .clear(),
            "claim" => claim.identity.name = "foreign".into(),
            "history" => {
                registry
                    .histories
                    .get_mut("example")
                    .ok_or("missing fixture history")?
                    .active_allocation = None;
            }
            "uid" => claim.uid = None,
            "replacement" => claim.uid = Some("replacement-uid".into()),
            "head" | "missing-resource" | "policy" | "api-type" => {}
            _ => return Err("unknown fixture corruption".into()),
        }
        let policy = policy(&registry.plan(TRANSITION_A)?.request.next.compiled);
        let (client, calls) = wire(vec![
            registry_object(&registry, REGISTRY_UID, "42")?,
            object("reakt-policy-example", &policy, "policy-uid", "44"),
        ]);
        let platform = registry.bind(client, "control", "workload", "cluster-a", TRANSITION_A)?;
        assert!(
            platform
                .publish_committed_inventory(&store, TRANSITION_A, "executor-a")
                .await
                .is_err(),
            "{corrupt}"
        );
        assert_eq!(store.load(&scope).await?, Some(published));
        let calls = calls.lock().map_err(|_| "calls lock poisoned")?;
        assert!(calls.len() <= 3);
        assert!(calls.iter().all(|call| call.0 == "GET"));
    }
    Ok(())
}

#[cfg(feature = "deployer")]
#[test]
fn ci_permission_report_distinguishes_creation_scope_and_runtime_authority()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (registry, _, _, _, _) = staged_initial_fixture()?;
    let plan = registry.plan(TRANSITION_A)?;
    let report = deployment_permissions(plan)?;
    let authority_access: Vec<_> = report
        .required_resources
        .iter()
        .filter(|access| access.namespace.as_deref() == Some(REGISTRY_AUTHORITY_NAMESPACE))
        .collect();
    assert_eq!(authority_access.len(), 1);
    let authority = authority_access.first().ok_or("authority access missing")?;
    assert_eq!(authority.verb, "get");
    assert_eq!(authority.resource, "configmaps");
    assert_eq!(
        authority.resource_names,
        ["reakt-registry-authority.control.workload"]
    );
    assert!(
        report
            .creation_targets
            .iter()
            .all(|target| target.namespace != REGISTRY_AUTHORITY_NAMESPACE)
    );

    for target in &report.creation_targets {
        assert!(report.required_resources.iter().any(|access| {
            access.namespace.as_deref() == Some(target.namespace.as_str())
                && access.api_group == target.api_group
                && access.resource == target.resource
                && access.verb == "create"
                && access.resource_names.is_empty()
        }));
    }
    assert!(
        report
            .creation_targets
            .iter()
            .any(|target| target.resource == "roles")
    );
    assert!(
        report
            .creation_targets
            .iter()
            .any(|target| target.resource == "deployments")
    );
    assert!(report.required_resources.iter().any(|access| {
        access.namespace.is_none() && access.resource == "nodes" && access.verb == "get"
    }));
    assert!(
        !report
            .required_resources
            .iter()
            .any(|access| access.resource == "pods/exec")
    );
    let delegated: Vec<_> = report
        .role_delegation
        .iter()
        .flat_map(|role| role.granted_capabilities.iter())
        .collect();
    assert_eq!(
        delegated.len(),
        plan.request.next.compiled.capabilities.len()
    );
    for capability in &plan.request.next.compiled.capabilities {
        assert!(delegated.contains(&capability));
    }
    let encoded = serde_json::to_string(&report)?;
    assert!(!encoded.contains("program.ts"));
    assert!(!encoded.contains("approved\""));
    assert!(encoded.contains("effective RBAC and admission not evaluated"));
    let mut altered = plan.clone();
    altered
        .generation_reservation
        .service_account_name
        .push_str("-changed");
    assert!(deployment_permissions(&altered).is_err());
    Ok(())
}

#[cfg(feature = "deployer")]
#[test]
fn upgrade_ci_permissions_cover_retirement_and_actual_exec_method()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let registry = staged_upgrade_fixture()?;
    let plan = registry.plan(TRANSITION_B)?;
    let report = deployment_permissions(plan)?;
    let previous = plan.request.predecessor()?.ok_or("missing predecessor")?;
    let account = previous.allocation.service_account_name.clone();
    for (resource, verb, name) in [
        ("serviceaccounts", "delete", account.clone()),
        ("rolebindings", "delete", account.clone()),
        ("deployments", "update", account),
        (
            "deployments",
            "update",
            format!("reakt-proxy-{}", previous.allocation.generation_id),
        ),
    ] {
        assert!(report.required_resources.iter().any(|access| {
            access.resource == resource
                && access.verb == verb
                && access.resource_names == vec![name.clone()]
        }));
    }
    let request = kube::core::Request::new("/api/v1/namespaces/control/pods").exec(
        "generated-proxy-pod",
        ["probe"],
        &kube::api::AttachParams::default(),
    )?;
    let verb = match *request.method() {
        http::Method::GET => "get",
        http::Method::POST => "create",
        _ => return Err("unexpected exec authorization method".into()),
    };
    assert!(report.required_resources.iter().any(|access| {
        access.resource == "pods/exec"
            && access.verb == verb
            && access.namespace.as_deref() == Some("control")
            && access.resource_names.is_empty()
    }));
    assert!(!report.creation_targets.iter().any(|target| {
        target.name == "reakt-activation-example" || target.name == "reakt-inventory-example"
    }));
    Ok(())
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn active_uid_capture_uses_inventory_and_rejects_missing_records()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    for remove_history in [false, true] {
        let mut registry = qualified_upgrade_fixture()?;
        registry.transitions.remove(TRANSITION_B);
        if remove_history {
            registry.plans.clear();
        }
        let snapshot: inventory::InventorySnapshot = serde_json::from_value(json!({
            "schemaVersion":"reakt.dev/resource-inventory/v1", "generation":GENERATION_A,
            "resources":[{"identity":{"apiGroup":"","resource":"configmaps","namespace":"workload","name":"config"},
                "apiVersion":"v1","kind":"ConfigMap","uid":null,"deletionPolicy":"orphan"}]
        }))?;
        let control = registry_object(&registry, REGISTRY_UID, "42")?;
        let live = json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{
            "name":"config","namespace":"workload","uid":"captured-uid","resourceVersion":"9",
            "managedFields":[{"manager":"reakt-example","operation":"Apply","apiVersion":"v1","fieldsType":"FieldsV1","fieldsV1":{}}]
        }});
        let (client, calls) = wire(vec![
            control.clone(),
            live,
            control.clone(),
            json!("echo-put"),
        ]);
        let captured = platform_client(client, "control", "workload", "cluster-a")?
            .capture_active_claim_uids("example", &snapshot)
            .await?;
        assert_eq!(
            captured
                .value
                .claims
                .first()
                .and_then(|claim| claim.uid.as_deref()),
            Some("captured-uid")
        );
        assert_eq!(calls.lock().map_err(|_| "request lock poisoned")?.len(), 5);
        let mut missing = snapshot;
        missing.resources.clear();
        let (client, calls) = wire(vec![control]);
        assert!(
            platform_client(client, "control", "workload", "cluster-a")?
                .capture_active_claim_uids("example", &missing)
                .await
                .is_err()
        );
        assert_eq!(calls.lock().map_err(|_| "request lock poisoned")?.len(), 2);
    }
    Ok(())
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn captured_uid_publication_preserves_metadata_and_uses_authoritative_claims()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    use inventory::{InventoryStore, LibsqlInventory};
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("inventory.db");
    let store = LibsqlInventory::open_local(&path).await?;
    let mut registry = qualified_upgrade_fixture()?;
    registry.transitions.remove(TRANSITION_B);
    let (scope, snapshot) =
        deploy::generation_inventory(&registry.plan(TRANSITION_A)?.request.next, &registry.claims)?;
    let head = store.publish(&scope, None, &snapshot).await?;
    registry.claims.first_mut().ok_or("claim missing")?.uid = Some("authoritative-uid".into());
    let mut captured = Versioned {
        value: registry.registry.clone(),
        uid: REGISTRY_UID.into(),
        resource_version: "42".into(),
    };
    captured
        .value
        .claims
        .first_mut()
        .ok_or("claim missing")?
        .uid = Some("forged-caller-uid".into());
    let state = registry_object(&registry, REGISTRY_UID, "42")?;
    let (client, calls) = wire(vec![state.clone(), state.clone(), state.clone()]);
    let published = platform_client(client, "control", "workload", "cluster-a")?
        .publish_captured_inventory(&store, "example", &captured, &head)
        .await?;
    let mut expected = snapshot;
    expected
        .resources
        .first_mut()
        .ok_or("resource missing")?
        .uid = Some("authoritative-uid".into());
    assert_eq!(published.snapshot, expected);
    assert!(
        calls
            .lock()
            .map_err(|_| "request lock poisoned")?
            .iter()
            .all(|request| request.0 == "GET")
    );
    let reopened = LibsqlInventory::open_local(&path).await?;
    assert_eq!(reopened.load(&scope).await?, Some(published.clone()));
    let (client, _) = wire(vec![state]);
    assert!(
        platform_client(client, "control", "workload", "cluster-a")?
            .publish_captured_inventory(&reopened, "example", &captured, &head)
            .await
            .is_err()
    );
    assert_eq!(reopened.load(&scope).await?, Some(published));
    Ok(())
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn captured_uid_publication_rejects_known_uid_change_without_overwrite()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    use inventory::{InventoryStore, LibsqlInventory};
    let directory = tempfile::tempdir()?;
    let store = LibsqlInventory::open_local(&directory.path().join("inventory.db")).await?;
    let mut registry = staged_upgrade_fixture()?;
    registry.transitions.remove(TRANSITION_B);
    registry.claims.first_mut().ok_or("claim missing")?.uid = Some("original-uid".into());
    let (scope, snapshot) =
        deploy::generation_inventory(&registry.plan(TRANSITION_A)?.request.next, &registry.claims)?;
    let head = store.publish(&scope, None, &snapshot).await?;
    registry.claims.first_mut().ok_or("claim missing")?.uid = Some("different-uid".into());
    let captured = Versioned {
        value: registry.registry.clone(),
        uid: REGISTRY_UID.into(),
        resource_version: "42".into(),
    };
    let (client, calls) = wire(vec![registry_object(&registry, REGISTRY_UID, "42")?]);
    assert!(
        platform_client(client, "control", "workload", "cluster-a")?
            .publish_captured_inventory(&store, "example", &captured, &head)
            .await
            .is_err()
    );
    assert_eq!(store.load(&scope).await?, Some(head));
    assert_eq!(calls.lock().map_err(|_| "request lock poisoned")?.len(), 2);
    Ok(())
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn inventory_checkpoint_rejects_tampered_policy_and_legacy_authority_before_capture()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    use inventory::{InventoryStore, LibsqlInventory};
    for corruption in ["policy", "legacy", "foreign-allocation"] {
        let directory = tempfile::tempdir()?;
        let store = LibsqlInventory::open_local(&directory.path().join("inventory.db")).await?;
        let mut registry = staged_upgrade_fixture()?;
        registry.transitions.remove(TRANSITION_B);
        let mut control = registry_object(&registry, REGISTRY_UID, "42")?;
        let (scope, mut snapshot) = deploy::generation_inventory(
            &registry.plan(TRANSITION_A)?.request.next,
            &registry.claims,
        )?;
        if corruption == "policy" {
            let resource = snapshot.resources.first_mut().ok_or("resource missing")?;
            resource.deletion_policy = match resource.deletion_policy {
                compiler_core::DeletionPolicy::Delete => compiler_core::DeletionPolicy::Orphan,
                compiler_core::DeletionPolicy::Orphan => compiler_core::DeletionPolicy::Delete,
            };
        } else {
            let history = registry
                .histories
                .get_mut("example")
                .ok_or("history missing")?;
            let allocation = history
                .active_allocation
                .as_mut()
                .ok_or("allocation missing")?;
            if corruption == "legacy" {
                allocation.inventory_metadata_digest = None;
            } else {
                allocation.generation_id = "foreign".into();
            }
            let encoded = control["data"]["state.json"]
                .as_str()
                .ok_or("registry state missing")?;
            let mut state: Value = serde_json::from_str(encoded)?;
            let allocation = state["histories"]["example"]["activeAllocation"]
                .as_object_mut()
                .ok_or("active allocation state missing")?;
            if corruption == "legacy" {
                allocation.remove("inventoryMetadataDigest");
            } else {
                allocation.insert("generationId".into(), json!("foreign"));
            }
            control["data"]["state.json"] = json!(serde_json::to_string(&state)?);
        }
        let head = store.publish(&scope, None, &snapshot).await?;
        let captured = Versioned {
            value: registry.registry.clone(),
            uid: REGISTRY_UID.into(),
            resource_version: "42".into(),
        };
        let (client, calls) = wire(vec![control.clone()]);
        assert!(
            platform_client(client, "control", "workload", "cluster-a")?
                .capture_active_claim_uids("example", &snapshot)
                .await
                .is_err(),
            "{corruption}"
        );
        assert_eq!(calls.lock().map_err(|_| "request lock poisoned")?.len(), 2);
        let (client, calls) = wire(vec![control]);
        assert!(
            platform_client(client, "control", "workload", "cluster-a")?
                .publish_captured_inventory(&store, "example", &captured, &head)
                .await
                .is_err(),
            "{corruption}"
        );
        assert_eq!(calls.lock().map_err(|_| "request lock poisoned")?.len(), 2);
        assert_eq!(store.load(&scope).await?, Some(head));
    }
    Ok(())
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn inventory_checkpoint_rejects_wrong_transition_claim_snapshot_before_capture()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    use inventory::{InventoryStore, LibsqlInventory};
    let directory = tempfile::tempdir()?;
    let store = LibsqlInventory::open_local(&directory.path().join("inventory.db")).await?;
    let mut registry = staged_upgrade_fixture()?;
    registry.transitions.remove(TRANSITION_B);
    let control = registry_object(&registry, REGISTRY_UID, "42")?;
    let mut corrupt_snapshot = registry.claim_snapshot()?;
    let (scope, snapshot) =
        deploy::generation_inventory(&registry.plan(TRANSITION_A)?.request.next, &registry.claims)?;
    let head = store.publish(&scope, None, &snapshot).await?;
    registry
        .claims
        .first_mut()
        .ok_or("claim missing")?
        .transition_id = "wrong-transition".into();
    corrupt_snapshot.claims.clone_from(&registry.claims);
    let captured = Versioned {
        value: registry.registry.clone(),
        uid: REGISTRY_UID.into(),
        resource_version: "42".into(),
    };
    let (client, calls) = wire(vec![control]);
    let platform = PlatformClient::new(client, "control", "workload", "cluster-a")?
        .with_claim_store(Arc::new(CorruptClaimStore {
            snapshot: corrupt_snapshot,
        }))
        .with_audit_store(Arc::new(audit_support::MemoryAuditStore::default()));
    assert!(
        platform
            .publish_captured_inventory(&store, "example", &captured, &head)
            .await
            .is_err(),
        "claim from another transition must not complete inventory UIDs"
    );
    assert_eq!(calls.lock().map_err(|_| "request lock poisoned")?.len(), 2);
    assert_eq!(store.load(&scope).await?, Some(head));
    Ok(())
}

#[cfg(feature = "deployer")]
fn qualified_upgrade_fixture() -> std::result::Result<LifecycleRegistry, Box<dyn std::error::Error>>
{
    let mut registry = staged_upgrade_fixture()?;
    let previous_plan = registry.plan(TRANSITION_A)?.clone();
    let previous = registry
        .transitions
        .get_mut(TRANSITION_A)
        .ok_or("predecessor missing")?;
    let binding = previous.runtime_binding.as_ref().ok_or("binding missing")?;
    let proxy_name = format!("reakt-proxy-{}", binding.generation_id);
    let configuration_name = format!("{proxy_name}-config");
    let qualification = digest(&"qualified-platform-environment")?;
    previous.request_drain_qualification = Some(RequestDrainQualification::IsolatedEnvoy {
        qualification_digest: qualification.clone(),
        node_image_digest: digest(&"qualified-node-image")?,
    });
    previous.proxy_identity = Some(ProxyIdentity {
        policy_digest: digest(&"proxy-policy")?,
        deployment_name: proxy_name.clone(),
        deployment_uid: "proxy-deployment-uid".into(),
        node_name: "worker".into(),
        node_uid: "worker-uid".into(),
        pod_name: "proxy-pod".into(),
        pod_uid: "proxy-pod-uid".into(),
        pod_spec_digest: digest(&"isolated-pod-spec")?,
        envoy_container_id: "containerd://envoy".into(),
        admin_container_id: "containerd://admin".into(),
        config_map_name: configuration_name.clone(),
        config_map_uid: "proxy-config-uid".into(),
        config_digest: digest(&"public-proxy-configuration")?,
        qualification_digest: qualification,
    });
    for (key, uid) in [
        (
            format!("v1/ServiceAccount/control/{}", binding.service_account_name),
            "credential-uid",
        ),
        (
            "v1/ConfigMap/control/reakt-inventory-example".into(),
            "inventory-uid",
        ),
        (
            format!("apps/v1/Deployment/control/{proxy_name}"),
            "proxy-deployment-uid",
        ),
        (
            format!("v1/ConfigMap/control/{configuration_name}"),
            "proxy-config-uid",
        ),
    ] {
        previous.staged_identity_uids.insert(key, uid.into());
    }
    for capability in &previous_plan.request.next.compiled.capabilities {
        previous.staged_identity_uids.insert(
            format!(
                "rbac.authorization.k8s.io/v1/RoleBinding/{}/{}",
                capability.namespace, binding.service_account_name
            ),
            format!("{}-binding-uid", capability.namespace),
        );
    }
    let checkpoint = fixture_writer_checkpoint(&registry)?;
    registry
        .active_writer_checkpoints
        .insert("example".into(), checkpoint.clone());
    registry
        .transitions
        .get_mut(TRANSITION_B)
        .ok_or("upgrade transition missing")?
        .previous_writer_checkpoint = Some(checkpoint);
    registry.admit(TRANSITION_B)?;
    Ok(registry)
}

#[cfg(feature = "deployer")]
fn fixture_writer_checkpoint(
    registry: &LifecycleRegistry,
) -> std::result::Result<WriterCheckpoint, Box<dyn std::error::Error>> {
    let (client, _) = wire(Vec::new());
    let platform = registry.bind(client, "control", "workload", "cluster-a", TRANSITION_A)?;
    let protected = Versioned {
        value: registry.registry.clone(),
        uid: REGISTRY_UID.into(),
        resource_version: "42".into(),
    };
    let selected = platform.selected(&protected, TRANSITION_A)?;
    Ok(crate::writer_checkpoint::project_committed(
        registry, &selected,
    )?)
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn writer_checkpoint_is_private_and_independent_of_old_history()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut registry = qualified_upgrade_fixture()?;
    let previous = registry
        .transitions
        .get_mut(TRANSITION_A)
        .ok_or("predecessor missing")?;
    let tls_key = format!(
        "v1/Secret/control/{}-tls",
        previous.allocation().service_account_name
    );
    previous
        .staged_identity_uids
        .insert(tls_key.clone(), "tls-uid".into());
    let unrelated = format!(
        "v1/ConfigMap/control/{}-config",
        previous.allocation().service_account_name
    );
    let public_digest = digest(&"public configuration fixture")?;
    previous
        .staged_content_digests
        .insert(unrelated.clone(), public_digest.clone());
    let policy = policy(&registry.plan(TRANSITION_B)?.request.next.compiled);
    let (client, calls) = stateful_wire(vec![
        registry_object(&registry, REGISTRY_UID, "42")?,
        object("reakt-policy-example", &policy, "policy-uid", "44"),
    ]);
    let platform = registry.bind(client, "control", "workload", "cluster-a", TRANSITION_B)?;
    let captured = platform
        .ensure_previous_writer_checkpoint(TRANSITION_B, "executor-b")
        .await?;
    let checkpoint = captured
        .value
        .transition(TRANSITION_B)?
        .previous_writer_checkpoint
        .as_ref()
        .ok_or("checkpoint absent")?;
    let encoded = serde_json::to_string(checkpoint)?;
    for forbidden in [
        "stagedContentDigests",
        "compiled",
        "desired",
        "private-hash-canary",
        "tls-uid",
    ] {
        assert!(!encoded.contains(forbidden));
    }
    let prior = captured.value.transition(TRANSITION_A)?;
    assert!(!prior.staged_content_digests.contains_key(&tls_key));
    assert_eq!(
        prior.staged_identity_uids.get(&tls_key).map(String::as_str),
        Some("tls-uid")
    );
    assert_eq!(
        prior
            .staged_content_digests
            .get(&unrelated)
            .map(String::as_str),
        Some(public_digest.as_str())
    );
    let again = platform
        .ensure_previous_writer_checkpoint(TRANSITION_B, "executor-b")
        .await?;
    assert_eq!(captured.value, again.value);
    {
        let calls = calls.lock().map_err(|_| "request lock poisoned")?;
        let writes: Vec<_> = calls
            .iter()
            .filter(|(method, _, _)| method == "PUT")
            .collect();
        assert!(writes.is_empty());
    }

    let mut compacted = LifecycleRegistry {
        registry: captured.value,
        plans: registry.plans.clone(),
    };
    compacted.plans.remove(TRANSITION_A);
    let (client, calls) = wire(vec![
        registry_object(&compacted, REGISTRY_UID, "43")?,
        object("reakt-policy-example", &policy, "policy-uid", "44"),
    ]);
    let restored = compacted
        .bind(client, "control", "workload", "cluster-a", TRANSITION_B)?
        .ensure_previous_writer_checkpoint(TRANSITION_B, "executor-b")
        .await?;
    assert_eq!(restored.value, compacted.registry);
    assert_eq!(calls.lock().map_err(|_| "request lock poisoned")?.len(), 3);
    Ok(())
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn missing_previous_writer_checkpoint_requires_explicit_recovery_without_writes()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut registry = qualified_upgrade_fixture()?;
    registry
        .transitions
        .get_mut(TRANSITION_B)
        .ok_or("upgrade transition missing")?
        .previous_writer_checkpoint = None;
    let policy = policy(&registry.plan(TRANSITION_B)?.request.next.compiled);
    let (client, calls) = wire(vec![
        registry_object(&registry, REGISTRY_UID, "42")?,
        object("reakt-policy-example", &policy, "policy-uid", "44"),
    ]);
    let platform = registry.bind(client, "control", "workload", "cluster-a", TRANSITION_B)?;
    assert!(
        platform
            .ensure_previous_writer_checkpoint(TRANSITION_B, "executor-b")
            .await
            .is_err()
    );
    assert!(
        calls
            .lock()
            .map_err(|_| "request lock poisoned")?
            .iter()
            .all(|(method, _, _)| method == "GET")
    );
    assert!(
        registry
            .transition(TRANSITION_B)?
            .previous_writer_checkpoint
            .is_none()
    );
    Ok(())
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn writer_checkpoint_substitution_never_overwrites_recorded_authority()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let registry = qualified_upgrade_fixture()?;
    let policy = policy(&registry.plan(TRANSITION_B)?.request.next.compiled);
    let (client, _) = stateful_wire(vec![
        registry_object(&registry, REGISTRY_UID, "42")?,
        object("reakt-policy-example", &policy, "policy-uid", "44"),
    ]);
    let platform = registry.bind(client, "control", "workload", "cluster-a", TRANSITION_B)?;
    let captured = platform
        .ensure_previous_writer_checkpoint(TRANSITION_B, "executor-b")
        .await?;
    for alteration in [
        "oversized",
        "scope",
        "generation",
        "proxy-name",
        "qualification",
        "extra-identity",
        "missing-identity",
    ] {
        let mut changed = LifecycleRegistry {
            registry: captured.value.clone(),
            plans: registry.plans.clone(),
        };
        let checkpoint = changed
            .transitions
            .get_mut(TRANSITION_B)
            .and_then(|stored| stored.previous_writer_checkpoint.as_mut())
            .ok_or("checkpoint absent")?;
        match alteration {
            "oversized" => checkpoint.runtime_binding.controller_image = "x".repeat(128 * 1024),
            "scope" => checkpoint.control_namespace = "other-control".into(),
            "generation" => checkpoint.runtime_binding.generation_id = "other-generation".into(),
            "proxy-name" => checkpoint.proxy_identity.deployment_name = "unrelated-proxy".into(),
            "qualification" => {
                checkpoint.proxy_identity.qualification_digest = digest(&"other-qualification")?;
            }
            "extra-identity" => {
                checkpoint.retirement_identity_uids.insert(
                    "v1/ServiceAccount/control/unrelated".into(),
                    "unrelated-uid".into(),
                );
            }
            "missing-identity" => {
                checkpoint.retirement_identity_uids.clear();
            }
            _ => return Err("unknown fixture alteration".into()),
        }
        let (client, calls) = wire(vec![
            registry_object(&changed, REGISTRY_UID, "43")?,
            object("reakt-policy-example", &policy, "policy-uid", "44"),
        ]);
        assert!(
            changed
                .bind(client, "control", "workload", "cluster-a", TRANSITION_B)?
                .ensure_previous_writer_checkpoint(TRANSITION_B, "executor-b")
                .await
                .is_err(),
            "{alteration}"
        );
        assert_eq!(calls.lock().map_err(|_| "request lock poisoned")?.len(), 2);
    }
    Ok(())
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn immutable_writer_checkpoint_rejects_changed_active_capability_commitment()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut registry = qualified_upgrade_fixture()?;
    registry
        .transitions
        .get_mut(TRANSITION_A)
        .and_then(|stored| stored.runtime_binding.as_mut())
        .ok_or("binding missing")?
        .capability_digest = digest(&"unapproved-capabilities")?;
    let policy = policy(&registry.plan(TRANSITION_B)?.request.next.compiled);
    let (client, calls) = wire(vec![
        registry_object(&registry, REGISTRY_UID, "42")?,
        object("reakt-policy-example", &policy, "policy-uid", "44"),
    ]);
    let platform = registry.bind(client, "control", "workload", "cluster-a", TRANSITION_B)?;
    assert!(
        platform
            .ensure_previous_writer_checkpoint(TRANSITION_B, "executor-b")
            .await
            .is_err()
    );
    assert_eq!(calls.lock().map_err(|_| "request lock poisoned")?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn recreated_registry_cannot_resume_copied_coordination_state()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (registry, _, _, _, _) = staged_initial_fixture()?;
    let (client, calls) = wire(vec![registry_object(
        &registry,
        "22222222-2222-4222-8222-222222222222",
        "42",
    )?]);
    let platform = platform_client(client, "control", "workload", "cluster-a")?;
    assert!(platform.load_registry().await.is_err());
    let recorded = calls.lock().map_err(|_| "wire calls lock poisoned")?;
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].0, "GET");
    Ok(())
}

#[test]
fn wire_fixture_identities_match_the_production_allocator()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    for (sequence, generation, transition) in [
        (1, GENERATION_A, TRANSITION_A),
        (2, GENERATION_B, TRANSITION_B),
    ] {
        let names = deploy::allocation_names(REGISTRY_UID, sequence)?;
        assert_eq!(names.generation_id, generation);
        assert_eq!(names.transition_id, transition);
    }
    Ok(())
}

#[tokio::test]
async fn exhausted_allocator_rejects_reservation_without_writes()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (mut registry, _, _, _, _) = staged_initial_fixture()?;
    let mut request = registry.plan(TRANSITION_A)?.request.clone();
    registry.next_sequence = u64::MAX;
    registry.claims.clear();
    registry.transitions.clear();
    request.registry.next_sequence = u64::MAX;
    let (client, calls) = wire(vec![registry_object(&registry, REGISTRY_UID, "42")?]);
    let platform = platform_client(client, "control", "workload", "cluster-a")?;
    let error = reserve_test_transition(&platform, &request, "executor-a")
        .await
        .err()
        .ok_or("exhausted reservation unexpectedly succeeded")?;
    assert!(
        error.to_string().contains("allocation counter exhausted"),
        "{error}"
    );
    let recorded = calls.lock().map_err(|_| "wire calls lock poisoned")?;
    assert!(recorded.iter().all(|(method, _, _)| method == "GET"));
    Ok(())
}

#[tokio::test]
async fn registry_authority_rejects_wrong_incarnation_scope_and_deletion()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (registry, _, _, _, _) = staged_initial_fixture()?;
    for corruption in [
        "registryUid",
        "controlNamespace",
        "workloadNamespace",
        "clusterBinding",
        "deleting",
    ] {
        let mut authority = registry_authority();
        if corruption == "deleting" {
            authority["metadata"]["deletionTimestamp"] = json!("2026-09-06T00:00:00Z");
        } else {
            let encoded = authority["data"]["state.json"]
                .as_str()
                .ok_or("authority state missing")?;
            let mut state: Value = serde_json::from_str(encoded)?;
            state[corruption] = json!("foreign");
            authority["data"]["state.json"] = json!(serde_json::to_string(&state)?);
        }
        let (client, calls) = wire_with_authority(
            vec![registry_object(&registry, REGISTRY_UID, "42")?],
            authority,
        );
        let platform = platform_client(client, "control", "workload", "cluster-a")?;
        assert!(platform.load_registry().await.is_err(), "{corruption}");
        let recorded = calls.lock().map_err(|_| "wire calls lock poisoned")?;
        assert_eq!(recorded.len(), 2);
        assert!(recorded.iter().all(|(method, _, _)| method == "GET"));
        assert!(
            recorded[1]
                .1
                .contains("/namespaces/reakt-platform/configmaps/")
        );
    }
    Ok(())
}

#[tokio::test]
async fn competing_reservation_cannot_overwrite_the_winning_allocation()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (winner, _, policy, _, _) = staged_initial_fixture()?;
    let request = winner.plan(TRANSITION_A)?.request.clone();
    let mut initial = winner.clone();
    initial.next_sequence = 1;
    initial.claims.clear();
    initial.transitions.clear();
    let (client, calls) = wire(vec![
        registry_object(&initial, REGISTRY_UID, "42")?,
        object("policy", &policy, "policy-uid", "40"),
        registry_object(&winner, REGISTRY_UID, "43")?,
    ]);
    let platform = platform_client(client, "control", "workload", "cluster-a")?;
    let error = reserve_test_transition(&platform, &request, "executor-b")
        .await
        .err()
        .ok_or("competing reservation unexpectedly succeeded")?;
    assert!(
        error.to_string().contains("UID/resourceVersion changed"),
        "{error}"
    );
    let recorded = calls.lock().map_err(|_| "wire calls lock poisoned")?;
    assert!(recorded.iter().all(|(method, _, _)| method == "GET"));
    Ok(())
}

#[tokio::test]
async fn another_domain_uses_the_registry_counter_after_domain_entry_removal()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (mut registry, _, _, _, _) = staged_initial_fixture()?;
    let mut request = registry.plan(TRANSITION_A)?.request.clone();
    registry.histories.remove("example");
    registry.transitions.clear();
    registry.claims.clear();
    let history = deploy::GenerationHistory {
        domain_id: "other".into(),
        cluster_binding: "cluster-a".into(),
        revision: "42".into(),
        active_allocation: None,
    };
    registry.histories.insert("other".into(), history.clone());
    let mut ir = request.next.compiled.ir.clone();
    ir.domain.id = "other".into();
    let resource = ir.resources.first_mut().ok_or("fixture resource missing")?;
    resource.name = "other-config".into();
    resource.desired["metadata"]["name"] = json!("other-config");
    request.next.compiled = compiler_core::validate(&ir)?;
    let policy = policy(&request.next.compiled);
    request.runtime_ceiling = policy.runtime_ceiling.clone();
    request.cleanup_ceiling = policy.cleanup_ceiling.clone();
    let names = deploy::allocation_names(REGISTRY_UID, registry.next_sequence)?;
    request.transition_id = names.transition_id.clone();
    request.next.generation_id = names.generation_id;
    request.registry = deploy::RegistrySnapshot {
        registry_uid: REGISTRY_UID.into(),
        next_sequence: registry.next_sequence,
        revision: "42".into(),
        claims: registry.claims.clone(),
        generation_history: history,
    };
    let (client, calls) = wire(vec![
        registry_object(&registry, REGISTRY_UID, "42")?,
        object("policy", &policy, "policy-uid", "40"),
        registry_object(&registry, REGISTRY_UID, "42")?,
        json!("echo-put"),
    ]);
    let platform = platform_client(client, "control", "workload", "cluster-a")?;
    let reserved = reserve_test_transition(&platform, &request, "executor-b").await?;
    assert_eq!(reserved.value.next_sequence, 3);
    assert_eq!(
        reserved
            .value
            .transition(&names.transition_id)?
            .allocation()
            .sequence,
        2
    );
    assert!(
        reserved.value.histories["other"]
            .active_allocation
            .is_none()
    );
    let recorded = calls.lock().map_err(|_| "wire calls lock poisoned")?;
    assert_eq!(
        recorded
            .iter()
            .filter(|(method, _, _)| method == "PUT")
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn active_writer_checkpoint_requires_retained_current_transition()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut registry = qualified_upgrade_fixture()?;
    registry.transitions.remove(TRANSITION_B);
    let (scope, inventory) =
        deploy::generation_inventory(&registry.plan(TRANSITION_A)?.request.next, &registry.claims)?;
    let (client, calls) = stateful_wire(vec![registry_object(&registry, REGISTRY_UID, "42")?]);
    let platform = platform_client(client, "control", "workload", "cluster-a")?;
    let captured = platform.ensure_active_writer_checkpoint("example").await?;
    let checkpoint = captured
        .value
        .active_writer_checkpoints
        .get("example")
        .ok_or("active writer checkpoint missing")?;
    let predecessor = checkpoint.predecessor_metadata(&scope, &inventory)?;
    assert_eq!(predecessor.allocation.generation_id, GENERATION_A);
    assert_eq!(
        predecessor.retirement_role_binding_namespaces,
        ["control", "workload"]
    );
    let bytes = serde_json::to_string(checkpoint)?;
    for excluded in ["compiled", "desired", "stagedContentDigests", "source"] {
        assert!(!bytes.contains(excluded), "{excluded}");
    }
    platform.ensure_active_writer_checkpoint("example").await?;
    {
        let recorded = calls.lock().map_err(|_| "wire calls lock poisoned")?;
        assert_eq!(
            recorded
                .iter()
                .filter(|(method, _, _)| method == "PUT")
                .count(),
            0
        );
    }
    let mut compacted = captured.value;
    compacted.transitions.clear();
    let (client, calls) = wire(vec![registry_object(&compacted, REGISTRY_UID, "43")?]);
    let platform = platform_client(client, "control", "workload", "cluster-a")?;
    assert!(
        platform
            .ensure_active_writer_checkpoint("example")
            .await
            .is_err()
    );
    let recorded = calls.lock().map_err(|_| "wire calls lock poisoned")?;
    assert!(recorded.iter().all(|(method, _, _)| method == "GET"));
    Ok(())
}

#[cfg(feature = "deployer")]
fn with_active_checkpoint(
    mut registry: LifecycleRegistry,
) -> std::result::Result<LifecycleRegistry, Box<dyn std::error::Error>> {
    let checkpoint = fixture_writer_checkpoint(&registry)?;
    registry
        .active_writer_checkpoints
        .insert("example".into(), checkpoint.clone());
    if let Some(stored) = registry.transitions.get_mut(TRANSITION_B) {
        stored.previous_writer_checkpoint = Some(checkpoint);
    }
    Ok(registry)
}

#[tokio::test]
async fn missing_or_changed_active_writer_checkpoint_requires_explicit_recovery()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut registry = qualified_upgrade_fixture()?;
    registry.active_writer_checkpoints.clear();
    let (client, calls) = wire(vec![registry_object(&registry, REGISTRY_UID, "42")?]);
    let platform = platform_client(client, "control", "workload", "cluster-a")?;
    let error = platform
        .ensure_active_writer_checkpoint("example")
        .await
        .err()
        .ok_or("missing checkpoint unexpectedly accepted")?;
    assert!(error.to_string().contains("explicit recovery"), "{error}");
    assert!(
        calls
            .lock()
            .map_err(|_| "calls lock poisoned")?
            .iter()
            .all(|(method, _, _)| method == "GET")
    );
    let mut changed = qualified_upgrade_fixture()?;
    changed
        .active_writer_checkpoints
        .get_mut("example")
        .ok_or("checkpoint missing")?
        .allocation
        .generation_id = "other-generation".into();
    let (client, calls) = wire(vec![registry_object(&changed, REGISTRY_UID, "42")?]);
    let platform = platform_client(client, "control", "workload", "cluster-a")?;
    let error = platform
        .ensure_active_writer_checkpoint("example")
        .await
        .err()
        .ok_or("changed checkpoint unexpectedly accepted")?;
    assert!(
        error.to_string().contains("allocation or scope differs"),
        "{error}"
    );
    assert!(
        calls
            .lock()
            .map_err(|_| "calls lock poisoned")?
            .iter()
            .all(|(method, _, _)| method == "GET")
    );
    Ok(())
}

#[tokio::test]
async fn unqualified_initial_writer_rejects_before_activation_mutations()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (registry, _, _, _, _) = staged_initial_fixture()?;
    let policy = policy(&registry.plan(TRANSITION_A)?.request.next.compiled);
    let (client, calls) = wire(vec![
        registry_object(&registry, REGISTRY_UID, "42")?,
        object("reakt-policy-example", &policy, "policy-uid", "44"),
    ]);
    let platform = registry.bind(client, "control", "workload", "cluster-a", TRANSITION_A)?;
    let error = Box::pin(platform.activate_initial(TRANSITION_A, "executor-a"))
        .await
        .err()
        .ok_or("unqualified writer unexpectedly activated")?;
    assert!(
        error
            .to_string()
            .contains("qualified isolated proxy before activation"),
        "{error}"
    );
    assert!(
        calls
            .lock()
            .map_err(|_| "calls lock poisoned")?
            .iter()
            .all(|(method, _, _)| method == "GET")
    );
    Ok(())
}
