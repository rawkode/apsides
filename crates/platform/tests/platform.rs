use compiler_core::{CompiledPlan, PermissionCeiling, digest, parse_and_validate};
use deploy::{ClaimState, ProtectedClaim, ResourceIdentity};
use http::{Request, Response};
use http_body_util::BodyExt;
use kube::{Client, client::Body};
use platform::*;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    convert::Infallible,
    sync::{Arc, Mutex},
};
use tower::service_fn;

type Calls = Arc<Mutex<Vec<(String, String, Value)>>>;
fn wire(responses: Vec<Value>) -> (Client, Calls) {
    let replies = Arc::new(Mutex::new(VecDeque::from(responses)));
    let calls: Calls = Arc::default();
    let service = service_fn({
        let calls = calls.clone();
        move |request: Request<Body>| {
            let replies = replies.clone();
            let calls = calls.clone();
            async move {
                let (parts, body) = request.into_parts();
                let data = body.collect().await.unwrap().to_bytes();
                let data: Value = if data.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&data).unwrap()
                };
                calls.lock().unwrap().push((
                    parts.method.to_string(),
                    parts.uri.to_string(),
                    data.clone(),
                ));
                let mut reply = replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected Kubernetes request");
                if reply == json!("echo-put") {
                    reply = data;
                    reply["metadata"]["resourceVersion"] = json!("43");
                }
                Ok::<_, Infallible>(
                    Response::builder()
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&reply).unwrap()))
                        .unwrap(),
                )
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
            generation_id: "generation-a".into(),
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
                generation_id: "generation-a".into(),
                transition_id: "transition-a".into(),
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
        let mut record = object("registry", &value, "registry-uid", "42");
        if value.is_object() {
            record["data"]["state.json"] = json!("{\"x\":1,\"x\":2}");
        }
        let (client, _) = wire(vec![record]);
        assert!(
            PlatformClient::new(client, "control", "workload", "cluster-a")
                .unwrap()
                .load_registry()
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn namespace_claim_reservation_is_one_conditional_update_and_rejects_stale_snapshot() {
    let compiled = plan();
    let policy = policy(&compiled);
    let history = deploy::GenerationHistory {
        domain_id: "example".into(),
        cluster_binding: "cluster-a".into(),
        revision: "42".into(),
        active_generation_id: None,
        allocations: vec![],
    };
    let registry = NamespaceRegistry {
        schema_version: SCHEMA_VERSION.into(),
        cluster_binding: "cluster-a".into(),
        workload_namespace: "workload".into(),
        claims: vec![],
        histories: BTreeMap::from([("example".into(), history.clone())]),
        transitions: BTreeMap::new(),
    };
    let request = deploy::DeploymentRequest {
        transition_id: "transition-a".into(),
        previous: None,
        next: deploy::GenerationPlan {
            generation_id: "generation-a".into(),
            artifact_digest: format!("sha256:{}", "a".repeat(64)),
            compiled,
        },
        runtime_ceiling: policy.runtime_ceiling.clone(),
        cleanup_ceiling: policy.cleanup_ceiling.clone(),
        registry: deploy::RegistrySnapshot {
            revision: "42".into(),
            claims: vec![],
            generation_history: history,
        },
        observations: vec![],
        ownership_intents: vec![],
    };
    let registry_object = object("registry", &registry, "registry-uid", "42");
    let (client, calls) = wire(vec![
        registry_object.clone(),
        object("policy", &policy, "policy-uid", "40"),
        registry_object,
        json!("echo-put"),
    ]);
    let platform = PlatformClient::new(client, "control", "workload", "cluster-a").unwrap();
    let changed = platform
        .reserve_transition(&request, "executor-a")
        .await
        .unwrap();
    assert_eq!(changed.value.claims.len(), 1);
    assert_eq!(changed.value.claims[0].state, ClaimState::Reserved);
    assert_eq!(changed.value.histories["example"].allocations.len(), 1);
    assert_eq!(
        changed.value.transitions["transition-a"].record.phase,
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
    let mut stale = request;
    stale.registry.revision = "41".into();
    let (client, calls) = wire(vec![object("registry", &registry, "registry-uid", "42")]);
    assert!(
        PlatformClient::new(client, "control", "workload", "cluster-a")
            .unwrap()
            .reserve_transition(&stale, "executor-b")
            .await
            .is_err()
    );
    assert_eq!(calls.lock().unwrap().len(), 1);
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
    let platform = PlatformClient::new(client, "control", "workload", "cluster-a").unwrap();
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
            PlatformClient::new(client, "control", "workload", "cluster-a")
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
    PlatformClient::new(client, "control", "workload", "cluster-a")
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
        PlatformClient::new(client, "control", "workload", "cluster-a")
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
        PlatformClient::new(client, "control", "workload", "cluster-a")
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
        PlatformClient::new(client, "control", "workload", "cluster-a")
            .unwrap()
            .observe_writer_termination(&expected_writers())
            .await
            .is_err()
    );
    assert_eq!(calls.lock().unwrap().len(), 3);
}

#[cfg(feature = "deployer")]
fn staged_initial_fixture() -> (
    NamespaceRegistry,
    ActivationState,
    PolicyState,
    Value,
    Value,
) {
    use deploy::{
        DeploymentRequest, Event, GenerationHistory, GenerationPlan, RegistrySnapshot, advance,
        begin, plan_transition,
    };
    let compiled = plan();
    let policy = policy(&compiled);
    let trust = json!({"platform":"approved"});
    let mut activation = activation(&compiled, &policy);
    activation.status = ActivationStatus::Inactive;
    let mut history = GenerationHistory {
        domain_id: "example".into(),
        cluster_binding: "cluster-a".into(),
        revision: "42".into(),
        active_generation_id: None,
        allocations: vec![],
    };
    let request = DeploymentRequest {
        transition_id: "transition-a".into(),
        previous: None,
        next: GenerationPlan {
            generation_id: "generation-a".into(),
            artifact_digest: activation.binding.build_digest.clone(),
            compiled,
        },
        runtime_ceiling: policy.runtime_ceiling.clone(),
        cleanup_ceiling: policy.cleanup_ceiling.clone(),
        registry: RegistrySnapshot {
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
            generation_id: "generation-a".into(),
            service_account_name: plan.generation_reservation.service_account_name.clone(),
            registry_cas_receipt_digest: digest(&"real fixture stage").unwrap(),
            verification_epoch: 0,
        },
    )
    .unwrap();
    history
        .allocations
        .push(plan.generation_reservation.clone());
    let deployment = json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":activation.binding.service_account_name,"namespace":"control","uid":"controller-uid","resourceVersion":"50"},"spec":{"replicas":0}});
    let deployment_key = format!(
        "apps/v1/Deployment/control/{}",
        activation.binding.service_account_name
    );
    let entry = StoredTransition {
        plan,
        record,
        staged_identity_uids: [
            (deployment_key.clone(), "controller-uid".into()),
            (
                "v1/ConfigMap/control/reakt-activation-example".into(),
                "activation-uid".into(),
            ),
        ]
        .into(),
        staged_content_digests: [(
            deployment_key,
            digest(&json!({"spec":{"replicas":0}})).unwrap(),
        )]
        .into(),
        runtime_binding: Some(activation.binding.clone()),
        staged_trust_policy_digest: Some(digest(&trust).unwrap()),
        staged_provenance_digest: Some(digest(&"verified provenance").unwrap()),
        request_drain_qualification: Some(RequestDrainQualification::UnqualifiedDirectApi),
        proxy_identity: None,
        upgrade_drain: None,
    };
    let mut claim = activation.approved_claims[0].claim.clone();
    claim.state = ClaimState::Reserved;
    let registry = NamespaceRegistry {
        schema_version: SCHEMA_VERSION.into(),
        cluster_binding: "cluster-a".into(),
        workload_namespace: "workload".into(),
        claims: vec![claim],
        histories: [("example".into(), history)].into(),
        transitions: [("transition-a".into(), entry)].into(),
    };
    (registry, activation, policy, trust, deployment)
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
    let store: Arc<Mutex<BTreeMap<String, Value>>> = Arc::new(Mutex::new(
        objects
            .into_iter()
            .map(|o| {
                let resource = match o["kind"].as_str().unwrap() {
                    "Deployment" => "deployments",
                    "ServiceAccount" => "serviceaccounts",
                    "Role" => "roles",
                    "RoleBinding" => "rolebindings",
                    _ => "configmaps",
                };
                (
                    format!(
                        "{}/{resource}/{}",
                        o["metadata"]["namespace"].as_str().unwrap(),
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
async fn initial_activation_starts_only_after_protected_authority_and_resumes_committed() {
    let (mut registry, activation, policy, trust, deployment) = staged_initial_fixture();
    let mut objects = initial_support_fixture(&mut registry);
    objects.extend(vec![
        object("reakt-registry-workload", &registry, "registry-uid", "42"),
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
    let platform = PlatformClient::new(client, "control", "workload", "cluster-a").unwrap();
    let committed = platform
        .activate_initial("transition-a", "executor-a")
        .await
        .unwrap();
    assert_eq!(
        committed.value.transitions["transition-a"].record.phase,
        deploy::Phase::Committed
    );
    assert_eq!(committed.value.claims[0].state, ClaimState::Active);
    let first = calls.lock().unwrap().clone();
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
    platform
        .activate_initial("transition-a", "executor-a")
        .await
        .unwrap();
    let subsequent = calls.lock().unwrap();
    assert!(subsequent[first.len()..].iter().all(|c| c.0 == "GET"));
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn initial_activation_rejects_changed_trust_without_writes() {
    let (registry, _, _, _, _) = staged_initial_fixture();
    let (client, calls) = wire(vec![
        object("reakt-registry-workload", &registry, "registry-uid", "42"),
        object(
            "reakt-trust-example",
            &json!({"platform":"changed"}),
            "trust-uid",
            "43",
        ),
    ]);
    assert!(
        PlatformClient::new(client, "control", "workload", "cluster-a")
            .unwrap()
            .activate_initial("transition-a", "executor-a")
            .await
            .is_err()
    );
    assert!(calls.lock().unwrap().iter().all(|c| c.0 == "GET"));
}

#[cfg(feature = "deployer")]
fn staged_upgrade_fixture() -> NamespaceRegistry {
    use deploy::{Event, Phase, Quiescence, RegistrySnapshot, advance, begin, plan_transition};
    let (mut registry, _, _, _, _) = staged_initial_fixture();
    let previous = registry.transitions.get_mut("transition-a").unwrap();
    for event in [
        Event::Quiesced {
            event_id: "initial-quiesce".into(),
            evidence: Quiescence::InitialInstallation {
                verification_epoch: 0,
            },
        },
        Event::Activated {
            event_id: "initial-active".into(),
            generation_id: "generation-a".into(),
            activation_receipt_digest: digest(&"active").unwrap(),
            verification_epoch: 0,
        },
        Event::Cleaning {
            event_id: "initial-cleaning".into(),
        },
        Event::Committed {
            event_id: "initial-commit".into(),
            cleanup_receipts: vec![],
            obsolete_identity_retirement_digest: digest(&"no predecessor").unwrap(),
        },
    ] {
        previous.record = advance(
            &previous.plan,
            &previous.record,
            previous.record.revision,
            "executor-a",
            event,
        )
        .unwrap();
    }
    assert_eq!(previous.record.phase, Phase::Committed);
    let previous = previous.clone();
    registry
        .histories
        .get_mut("example")
        .unwrap()
        .active_generation_id = Some("generation-a".into());
    for claim in &mut registry.claims {
        claim.state = ClaimState::Active;
    }
    let mut request = previous.plan.request.clone();
    request.transition_id = "transition-b".into();
    request.previous = Some(previous.plan.request.next.clone());
    request.next.generation_id = "generation-b".into();
    request.registry = RegistrySnapshot {
        revision: "42".into(),
        claims: registry.claims.clone(),
        generation_history: registry.histories["example"].clone(),
    };
    let plan = plan_transition(&request).unwrap();
    let mut current = previous;
    current.record = begin(&plan, "executor-b").unwrap();
    current.record = advance(
        &plan,
        &current.record,
        0,
        "executor-b",
        Event::Staged {
            event_id: "stage-b".into(),
            generation_id: "generation-b".into(),
            service_account_name: plan.generation_reservation.service_account_name.clone(),
            registry_cas_receipt_digest: digest(&"new source staging").unwrap(),
            verification_epoch: 0,
        },
    )
    .unwrap();
    current.plan = plan;
    current.runtime_binding.as_mut().unwrap().generation_id = "generation-b".into();
    registry.transitions.insert("transition-b".into(), current);
    registry
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn direct_api_predecessor_is_durably_blocked_and_forged_phase_never_writes() {
    use deploy::Phase;
    let registry = staged_upgrade_fixture();
    let (client, calls) = wire(vec![
        object("reakt-registry-workload", &registry, "registry-uid", "42"),
        object("reakt-registry-workload", &registry, "registry-uid", "42"),
        object("reakt-registry-workload", &registry, "registry-uid", "42"),
        json!("echo-put"),
    ]);
    let platform = PlatformClient::new(client, "control", "workload", "cluster-a").unwrap();
    assert!(
        platform
            .quiesce_previous("transition-b", "executor-b")
            .await
            .is_err()
    );
    assert_eq!(calls.lock().unwrap().len(), 4);
    assert_eq!(calls.lock().unwrap()[3].0, "PUT");
    let blocked: NamespaceRegistry = serde_json::from_str(
        calls.lock().unwrap()[3].2["data"]["state.json"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        blocked.transitions["transition-b"].record.phase,
        Phase::Blocked
    );
    assert_eq!(calls.lock().unwrap()[0].0, "GET");

    for phase in [Phase::Quiesced, Phase::Activated, Phase::Cleaning] {
        let mut forged = registry.clone();
        forged
            .transitions
            .get_mut("transition-b")
            .unwrap()
            .record
            .phase = phase;
        let (client, calls) = wire(vec![object(
            "reakt-registry-workload",
            &forged,
            "registry-uid",
            "42",
        )]);
        let platform = PlatformClient::new(client, "control", "workload", "cluster-a").unwrap();
        assert!(
            platform
                .activate_upgrade("transition-b", "executor-b")
                .await
                .is_err()
        );
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert_eq!(calls.lock().unwrap()[0].0, "GET");
    }
}

#[cfg(feature = "deployer")]
fn initial_support_fixture(registry: &mut NamespaceRegistry) -> Vec<Value> {
    let stored = registry.transitions.get_mut("transition-a").unwrap();
    let name = stored
        .plan
        .generation_reservation
        .service_account_name
        .clone();
    let annotations = json!({"reakt.dev/transition":"transition-a","reakt.dev/generation":"generation-a","reakt.dev/plan-digest":stored.plan.plan_digest});
    let mut objects = vec![
        json!({"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":name,"namespace":"control","annotations":annotations},"automountServiceAccountToken":false}),
    ];
    let mut rules: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for capability in &stored.plan.request.next.compiled.capabilities {
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
    objects
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn broadened_staged_role_and_recreated_config_block_before_activation() {
    for changed_kind in ["Role", "ConfigMap"] {
        let (mut registry, activation, policy, trust, deployment) = staged_initial_fixture();
        let mut objects = initial_support_fixture(&mut registry);
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
            object("reakt-registry-workload", &registry, "registry-uid", "42"),
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
        let platform = PlatformClient::new(client, "control", "workload", "cluster-a").unwrap();
        assert!(
            platform
                .activate_initial("transition-a", "executor-a")
                .await
                .is_err()
        );
        assert!(calls.lock().unwrap().iter().all(|c| c.0 == "GET"));
    }
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn crash_after_start_resume_rechecks_authority_before_replica_one_fast_path() {
    use deploy::{Event, Quiescence, advance};
    let (mut registry, activation, policy, trust, mut deployment) = staged_initial_fixture();
    let entry = registry.transitions.get_mut("transition-a").unwrap();
    for event in [
        Event::Quiesced {
            event_id: "q".into(),
            evidence: Quiescence::InitialInstallation {
                verification_epoch: 0,
            },
        },
        Event::Activated {
            event_id: "a".into(),
            generation_id: "generation-a".into(),
            activation_receipt_digest: digest(&"real activation").unwrap(),
            verification_epoch: 0,
        },
    ] {
        entry.record = advance(
            &entry.plan,
            &entry.record,
            entry.record.revision,
            "executor-a",
            event,
        )
        .unwrap();
    }
    deployment["spec"]["replicas"] = json!(1);
    let mut objects = initial_support_fixture(&mut registry);
    objects.extend(vec![
        object("reakt-registry-workload", &registry, "registry-uid", "42"),
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
    let platform = PlatformClient::new(client, "control", "workload", "cluster-a").unwrap();
    assert!(
        platform
            .activate_initial("transition-a", "executor-a")
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
}

fn adoption_request() -> (NamespaceRegistry, PolicyState, deploy::DeploymentRequest) {
    let compiled = plan();
    let policy = policy(&compiled);
    let history = deploy::GenerationHistory {
        domain_id: "example".into(),
        cluster_binding: "cluster-a".into(),
        revision: "42".into(),
        active_generation_id: None,
        allocations: vec![],
    };
    let registry = NamespaceRegistry {
        schema_version: SCHEMA_VERSION.into(),
        cluster_binding: "cluster-a".into(),
        workload_namespace: "workload".into(),
        claims: vec![],
        histories: BTreeMap::from([("example".into(), history.clone())]),
        transitions: BTreeMap::new(),
    };
    let next = deploy::GenerationPlan {
        generation_id: "generation-a".into(),
        artifact_digest: format!("sha256:{}", "a".repeat(64)),
        compiled,
    };
    let approval: deploy::OwnershipApproval = serde_json::from_value(json!({
        "schemaVersion":"reakt.dev/ownership-intent/v1", "transitionId":"transition-a",
        "domainId":"example", "generationId":"generation-a", "releaseDigest":next.artifact_digest,
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
        transition_id: "transition-a".into(),
        previous: None,
        next,
        runtime_ceiling: policy.runtime_ceiling.clone(),
        cleanup_ceiling: policy.cleanup_ceiling.clone(),
        registry: deploy::RegistrySnapshot {
            revision: "42".into(),
            claims: vec![],
            generation_history: history,
        },
        observations: vec![],
        ownership_intents: vec![intent],
    };
    (registry, policy, request)
}

#[tokio::test]
async fn adoption_reservation_rechecks_operator_approval_and_uses_non_force_uid_bound_dry_run() {
    let (registry, policy, request) = adoption_request();
    let approval = &request.ownership_intents[0].approval;
    for corruption in [
        "none",
        "approval-uid",
        "object-uid",
        "object-version",
        "managed-fields",
        "dry-run-uid",
    ] {
        let registry_object = object("registry", &registry, "registry-uid", "42");
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
        let result = PlatformClient::new(client, "control", "workload", "cluster-a")
            .unwrap()
            .reserve_transition(&request, "executor-a")
            .await;
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls[2].1,
            "/api/v1/namespaces/reakt-platform-intents/configmaps/approve-config"
        );
        if corruption == "none" {
            let reserved = result.unwrap();
            assert_eq!(
                reserved.value.claims[0].uid.as_deref(),
                Some("existing-uid")
            );
            let patch = &calls[4];
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
}

#[cfg(feature = "deployer")]
fn committed_inventory_fixture() -> NamespaceRegistry {
    use deploy::{Event, Quiescence, advance};
    let (mut registry, _, _, _, _) = staged_initial_fixture();
    let stored = registry.transitions.get_mut("transition-a").unwrap();
    for event in [
        Event::Quiesced {
            event_id: "quiesce".into(),
            evidence: Quiescence::InitialInstallation {
                verification_epoch: 0,
            },
        },
        Event::Activated {
            event_id: "activate".into(),
            generation_id: "generation-a".into(),
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
            &stored.plan,
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
        .active_generation_id = Some("generation-a".into());
    registry.claims[0].state = ClaimState::Active;
    registry.claims[0].uid = Some("observed-uid".into());
    registry
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn committed_inventory_persists_metadata_and_retries_after_reopen() {
    use inventory::{InventoryStore, LibsqlInventory};
    let registry = committed_inventory_fixture();
    let response = object("reakt-registry-workload", &registry, "registry-uid", "42");
    let (client, calls) = wire(vec![response; 6]);
    let platform = PlatformClient::new(client, "control", "workload", "cluster-a").unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("inventory.db");
    let stored = {
        let store = LibsqlInventory::open_local(&path).await.unwrap();
        platform
            .publish_committed_inventory(&store, "transition-a", "executor-a")
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
            .publish_committed_inventory(&reopened, "transition-a", "executor-a")
            .await
            .unwrap(),
        stored
    );
    let scope = deploy::generation_inventory(
        &registry.transitions["transition-a"].plan.request.next,
        &registry.claims,
    )
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
    assert_eq!(calls.len(), 6);
    assert!(
        calls
            .iter()
            .all(|call| call.0 == "GET" && call.1.ends_with("/configmaps/reakt-registry-workload"))
    );
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn registry_races_do_not_report_inventory_completion() {
    use inventory::{InventoryStore, LibsqlInventory};
    let registry = committed_inventory_fixture();
    let scope = deploy::generation_inventory(
        &registry.transitions["transition-a"].plan.request.next,
        &registry.claims,
    )
    .unwrap()
    .0;
    let stable = object("reakt-registry-workload", &registry, "registry-uid", "42");
    let changed = object("reakt-registry-workload", &registry, "registry-uid", "43");
    for after_commit in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let store = LibsqlInventory::open_local(&directory.path().join("inventory.db"))
            .await
            .unwrap();
        let replies = if after_commit {
            vec![stable.clone(), stable.clone(), changed.clone()]
        } else {
            vec![stable.clone(), changed.clone()]
        };
        let (client, _) = wire(replies);
        let platform = PlatformClient::new(client, "control", "workload", "cluster-a").unwrap();
        assert!(
            platform
                .publish_committed_inventory(&store, "transition-a", "executor-a")
                .await
                .unwrap_err()
                .to_string()
                .contains("registry changed")
        );
        assert_eq!(store.load(&scope).await.unwrap().is_some(), after_commit);
        let (client, _) = wire(vec![changed.clone(); 3]);
        let platform = PlatformClient::new(client, "control", "workload", "cluster-a").unwrap();
        assert!(
            platform
                .publish_committed_inventory(&store, "transition-a", "executor-a")
                .await
                .is_ok()
        );
    }
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
        let mut registry = committed_inventory_fixture();
        let (scope, mut snapshot) = deploy::generation_inventory(
            &registry
                .transitions
                .get("transition-a")
                .ok_or("missing fixture transition")?
                .plan
                .request
                .next,
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
                .get_mut("transition-a")
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
                    .active_generation_id = Some("another".into());
            }
            "uid" => claim.uid = None,
            "replacement" => claim.uid = Some("replacement-uid".into()),
            "head" | "missing-resource" | "policy" | "api-type" => {}
            _ => return Err("unknown fixture corruption".into()),
        }
        let (client, calls) = wire(vec![object(
            "reakt-registry-workload",
            &registry,
            "registry-uid",
            "42",
        )]);
        let platform = PlatformClient::new(client, "control", "workload", "cluster-a")?;
        assert!(
            platform
                .publish_committed_inventory(&store, "transition-a", "executor-a")
                .await
                .is_err(),
            "{corrupt}"
        );
        assert_eq!(store.load(&scope).await?, Some(published));
        assert_eq!(calls.lock().map_err(|_| "calls lock poisoned")?.len(), 1);
    }
    Ok(())
}

#[cfg(feature = "deployer")]
#[test]
fn ci_permission_report_distinguishes_creation_scope_and_runtime_authority()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (registry, _, _, _, _) = staged_initial_fixture();
    let plan = &registry.transition("transition-a")?.plan;
    let report = deployment_permissions(plan)?;
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
    let registry = staged_upgrade_fixture();
    let plan = &registry.transition("transition-b")?.plan;
    let report = deployment_permissions(plan)?;
    let previous = plan
        .request
        .previous
        .as_ref()
        .ok_or("missing predecessor")?;
    let account = deploy::service_account_name(previous)?;
    for (resource, verb, name) in [
        ("serviceaccounts", "delete", account.clone()),
        ("rolebindings", "delete", account.clone()),
        ("deployments", "update", account),
        (
            "deployments",
            "update",
            format!("reakt-proxy-{}", previous.generation_id),
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
    let mut registry = staged_upgrade_fixture();
    registry.transitions.remove("transition-b");
    let snapshot: inventory::InventorySnapshot = serde_json::from_value(json!({
        "schemaVersion":"reakt.dev/resource-inventory/v1", "generation":"generation-a",
        "resources":[{"identity":{"apiGroup":"","resource":"configmaps","namespace":"workload","name":"config"},
            "apiVersion":"v1","kind":"ConfigMap","uid":null,"deletionPolicy":"orphan"}]
    }))?;
    let control = object("reakt-registry-workload", &registry, "registry-uid", "42");
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
    let captured = PlatformClient::new(client, "control", "workload", "cluster-a")?
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
    assert_eq!(calls.lock().map_err(|_| "request lock poisoned")?.len(), 4);
    let mut missing = snapshot;
    missing.resources.clear();
    let (client, calls) = wire(vec![control]);
    assert!(
        PlatformClient::new(client, "control", "workload", "cluster-a")?
            .capture_active_claim_uids("example", &missing)
            .await
            .is_err()
    );
    assert_eq!(calls.lock().map_err(|_| "request lock poisoned")?.len(), 1);
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
    let mut registry = staged_upgrade_fixture();
    registry.transitions.remove("transition-b");
    let (scope, snapshot) = deploy::generation_inventory(
        &registry.transition("transition-a")?.plan.request.next,
        &registry.claims,
    )?;
    let head = store.publish(&scope, None, &snapshot).await?;
    registry.claims.first_mut().ok_or("claim missing")?.uid = Some("authoritative-uid".into());
    let mut captured = Versioned {
        value: registry.clone(),
        uid: "registry-uid".into(),
        resource_version: "42".into(),
    };
    captured
        .value
        .claims
        .first_mut()
        .ok_or("claim missing")?
        .uid = Some("forged-caller-uid".into());
    let state = object("reakt-registry-workload", &registry, "registry-uid", "42");
    let (client, calls) = wire(vec![state.clone(), state.clone(), state.clone()]);
    let published = PlatformClient::new(client, "control", "workload", "cluster-a")?
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
        PlatformClient::new(client, "control", "workload", "cluster-a")?
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
    let mut registry = staged_upgrade_fixture();
    registry.transitions.remove("transition-b");
    registry.claims.first_mut().ok_or("claim missing")?.uid = Some("original-uid".into());
    let (scope, snapshot) = deploy::generation_inventory(
        &registry.transition("transition-a")?.plan.request.next,
        &registry.claims,
    )?;
    let head = store.publish(&scope, None, &snapshot).await?;
    registry.claims.first_mut().ok_or("claim missing")?.uid = Some("different-uid".into());
    let captured = Versioned {
        value: registry.clone(),
        uid: "registry-uid".into(),
        resource_version: "42".into(),
    };
    let (client, calls) = wire(vec![object(
        "reakt-registry-workload",
        &registry,
        "registry-uid",
        "42",
    )]);
    assert!(
        PlatformClient::new(client, "control", "workload", "cluster-a")?
            .publish_captured_inventory(&store, "example", &captured, &head)
            .await
            .is_err()
    );
    assert_eq!(store.load(&scope).await?, Some(head));
    assert_eq!(calls.lock().map_err(|_| "request lock poisoned")?.len(), 1);
    Ok(())
}

#[cfg(feature = "deployer")]
#[tokio::test]
async fn inventory_checkpoint_rejects_tampered_policy_and_legacy_authority_before_capture()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    use inventory::{InventoryStore, LibsqlInventory};
    for corruption in ["policy", "legacy", "duplicate-allocation"] {
        let directory = tempfile::tempdir()?;
        let store = LibsqlInventory::open_local(&directory.path().join("inventory.db")).await?;
        let mut registry = staged_upgrade_fixture();
        registry.transitions.remove("transition-b");
        let (scope, mut snapshot) = deploy::generation_inventory(
            &registry.transition("transition-a")?.plan.request.next,
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
                .allocations
                .iter_mut()
                .find(|entry| entry.generation_id == snapshot.generation)
                .ok_or("allocation missing")?;
            if corruption == "legacy" {
                allocation.inventory_metadata_digest = None;
            } else {
                let duplicate = allocation.clone();
                history.allocations.push(duplicate);
            }
        }
        let head = store.publish(&scope, None, &snapshot).await?;
        let captured = Versioned {
            value: registry.clone(),
            uid: "registry-uid".into(),
            resource_version: "42".into(),
        };
        let control = object("reakt-registry-workload", &registry, "registry-uid", "42");
        let (client, calls) = wire(vec![control.clone()]);
        assert!(
            PlatformClient::new(client, "control", "workload", "cluster-a")?
                .capture_active_claim_uids("example", &snapshot)
                .await
                .is_err(),
            "{corruption}"
        );
        assert_eq!(calls.lock().map_err(|_| "request lock poisoned")?.len(), 1);
        let (client, calls) = wire(vec![control]);
        assert!(
            PlatformClient::new(client, "control", "workload", "cluster-a")?
                .publish_captured_inventory(&store, "example", &captured, &head)
                .await
                .is_err(),
            "{corruption}"
        );
        assert_eq!(calls.lock().map_err(|_| "request lock poisoned")?.len(), 1);
        assert_eq!(store.load(&scope).await?, Some(head));
    }

    let directory = tempfile::tempdir()?;
    let store = LibsqlInventory::open_local(&directory.path().join("inventory.db")).await?;
    let mut registry = staged_upgrade_fixture();
    registry.transitions.remove("transition-b");
    let (scope, snapshot) = deploy::generation_inventory(
        &registry.transition("transition-a")?.plan.request.next,
        &registry.claims,
    )?;
    let head = store.publish(&scope, None, &snapshot).await?;
    registry
        .claims
        .first_mut()
        .ok_or("claim missing")?
        .transition_id = "wrong-transition".into();
    let captured = Versioned {
        value: registry.clone(),
        uid: "registry-uid".into(),
        resource_version: "42".into(),
    };
    let (client, calls) = wire(vec![object(
        "reakt-registry-workload",
        &registry,
        "registry-uid",
        "42",
    )]);
    assert!(
        PlatformClient::new(client, "control", "workload", "cluster-a")?
            .publish_captured_inventory(&store, "example", &captured, &head)
            .await
            .is_err(),
        "claim from another transition must not complete inventory UIDs"
    );
    assert_eq!(calls.lock().map_err(|_| "request lock poisoned")?.len(), 1);
    assert_eq!(store.load(&scope).await?, Some(head));
    Ok(())
}
