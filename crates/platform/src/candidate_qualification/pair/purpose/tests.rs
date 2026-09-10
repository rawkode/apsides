use super::*;
use compiler_core::{Capability, PermissionCeiling, parse_and_validate};
use deploy::{
    ClaimState, DeploymentRequest, GenerationAllocation, GenerationHistory, GenerationPlan,
    ProtectedClaim, RegistrySnapshot, ResourceIdentity,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const REGISTRY_UID: &str = "11111111-1111-4111-8111-111111111111";
const INITIAL_DIGEST: &str =
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SUCCESSOR_DIGEST: &str =
    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[derive(Clone, Copy)]
struct ResourceFixture {
    id: &'static str,
    kind: &'static str,
    deletion_policy: &'static str,
}

fn resource(fixture: ResourceFixture) -> Value {
    let desired = match fixture.kind {
        "Service" => json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": {"name": fixture.id, "namespace": "workload"},
            "spec": {
                "selector": {"app": "fixture"},
                "ports": [{"port": 80, "targetPort": 8080}],
                "type": "ClusterIP"
            }
        }),
        "Deployment" => json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {"name": fixture.id, "namespace": "workload"},
            "spec": {
                "replicas": 1,
                "selector": {"matchLabels": {"app": "fixture"}},
                "template": {
                    "metadata": {"labels": {"app": "fixture"}},
                    "spec": {
                        "serviceAccountName": "fixture-workload",
                        "containers": [{
                            "name": "fixture",
                            "image": "example.invalid/fixture@sha256:0000000000000000000000000000000000000000000000000000000000000000"
                        }]
                    }
                }
            }
        }),
        _ => json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": fixture.id, "namespace": "workload"},
            "data": {"value": "fixture"}
        }),
    };
    json!({
        "id": fixture.id,
        "apiVersion": if fixture.kind == "Deployment" { "apps/v1" } else { "v1" },
        "kind": fixture.kind,
        "name": fixture.id,
        "namespace": "workload",
        "desired": desired,
        "lifecycle": {"deletionPolicy": fixture.deletion_policy},
        "source": {"file": "program.ts", "line": 1, "column": 1}
    })
}

fn compiled(resources: &[ResourceFixture]) -> TestResult<CompiledPlan> {
    compiled_with_behavior(resources, &[], &[])
}

fn compiled_with_behavior(
    resources: &[ResourceFixture],
    inputs: &[Value],
    reactions: &[Value],
) -> TestResult<CompiledPlan> {
    let resources = resources.iter().copied().map(resource).collect::<Vec<_>>();
    Ok(parse_and_validate(
        &json!({
            "schemaVersion": "apsides.dev/ir/v1",
            "testOnly": false,
            "domain": {
                "id": "example",
                "namespace": "workload",
                "controlNamespace": "control",
                "clusterBinding": "cluster-a",
                "policyRef": "platform/example",
                "enforcement": "api-bound",
                "budgets": {
                    "maxResources": 16,
                    "maxInputs": 16,
                    "maxReactions": 16,
                    "maxObjectBytes": 262_144,
                    "maxQueueDepth": 16,
                    "maxConcurrentRequests": 4
                }
            },
            "resources": resources,
            "inputs": inputs,
            "reactions": reactions,
            "provenance": {
                "compiler": "apsides/0.1",
                "inputs": [{
                    "path": "program.ts",
                    "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
                }]
            }
        })
        .to_string(),
    )?)
}

fn reactive_cleanup_pair() -> TestResult<(CompiledPlan, CompiledPlan)> {
    let deployment = ResourceFixture {
        id: "common-deployment",
        kind: "Deployment",
        deletion_policy: "orphan",
    };
    Ok((
        compiled_with_behavior(
            &[
                ResourceFixture {
                    id: "cleanup-target",
                    kind: "Service",
                    deletion_policy: "delete",
                },
                deployment,
            ],
            &[json!({
                "id": "initial-input",
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "name": "initial-input",
                "namespace": "workload",
                "mode": "observed",
                "projection": ["data"],
                "sensitive": false,
                "missing": "block-dependent-mutations",
                "source": {"file": "program.ts", "line": 2, "column": 1}
            })],
            &[json!({
                "id": "initial-reaction",
                "op": "rollout",
                "inputs": ["initial-input"],
                "target": "common-deployment",
                "source": {"file": "program.ts", "line": 3, "column": 1}
            })],
        )?,
        compiled(&[deployment])?,
    ))
}

fn cleanup_pair(target_policy: &'static str) -> TestResult<(CompiledPlan, CompiledPlan)> {
    Ok((
        compiled(&[
            ResourceFixture {
                id: "cleanup-target",
                kind: "Service",
                deletion_policy: target_policy,
            },
            ResourceFixture {
                id: "common-config",
                kind: "ConfigMap",
                deletion_policy: "orphan",
            },
        ])?,
        compiled(&[ResourceFixture {
            id: "common-config",
            kind: "ConfigMap",
            deletion_policy: "orphan",
        }])?,
    ))
}

fn identity(compiled: &CompiledPlan, id: &str) -> TestResult<ResourceIdentity> {
    let resource = compiled
        .ir
        .resources
        .iter()
        .find(|resource| resource.id == id)
        .ok_or("fixture resource is absent")?;
    Ok(deploy::resource_identity(resource)?)
}

fn purpose(initial: &CompiledPlan) -> TestResult<CandidatePairPurpose> {
    Ok(CandidatePairPurpose::LastTypeCleanup {
        identity: identity(initial, "cleanup-target")?,
    })
}

fn generation(
    compiled: CompiledPlan,
    sequence: u64,
    artifact_digest: &str,
) -> TestResult<GenerationPlan> {
    let names = deploy::allocation_names(REGISTRY_UID, sequence)?;
    Ok(GenerationPlan {
        generation_id: names.generation_id,
        artifact_digest: artifact_digest.into(),
        compiled,
    })
}

fn ceiling(
    initial: &CompiledPlan,
    successor: &CompiledPlan,
    target: &ResourceIdentity,
) -> PermissionCeiling {
    let domain = &successor.ir.domain;
    let mut capabilities = initial
        .capabilities
        .iter()
        .chain(&successor.capabilities)
        .cloned()
        .collect::<BTreeSet<_>>();
    for verb in ["get", "delete"] {
        capabilities.insert(Capability {
            namespace: target.namespace.clone(),
            api_group: target.api_group.clone(),
            resource: target.resource.clone(),
            subresource: None,
            verb: verb.into(),
            names: vec![target.name.clone()],
        });
    }
    PermissionCeiling {
        policy_ref: domain.policy_ref.clone(),
        domain_id: domain.id.clone(),
        cluster_binding: domain.cluster_binding.clone(),
        workload_namespace: domain.namespace.clone(),
        control_namespace: domain.control_namespace.clone(),
        capabilities: capabilities.into_iter().collect(),
        accepted_enforcement_gaps: initial
            .enforcement_gaps
            .iter()
            .chain(&successor.enforcement_gaps)
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        accepted_indirect_risks: initial
            .indirect_risks
            .iter()
            .chain(&successor.indirect_risks)
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    }
}

fn cleanup_plan(
    initial: CompiledPlan,
    successor: CompiledPlan,
) -> TestResult<deploy::TransitionPlan> {
    let target = identity(&initial, "cleanup-target")?;
    let initial = generation(initial, 1, INITIAL_DIGEST)?;
    let successor = generation(successor, 2, SUCCESSOR_DIGEST)?;
    let initial_names = deploy::allocation_names(REGISTRY_UID, 1)?;
    let successor_names = deploy::allocation_names(REGISTRY_UID, 2)?;
    let claims = initial
        .compiled
        .ir
        .resources
        .iter()
        .map(|resource| {
            Ok(ProtectedClaim {
                identity: deploy::resource_identity(resource)?,
                domain_id: "example".into(),
                generation_id: initial.generation_id.clone(),
                transition_id: initial_names.transition_id.clone(),
                state: ClaimState::Active,
                uid: Some(format!("uid-{}", resource.id)),
            })
        })
        .collect::<std::result::Result<Vec<_>, deploy::Error>>()?;
    let (scope, inventory) = deploy::generation_inventory(&initial, &claims)?;
    let inventory_metadata_digest = deploy::inventory_metadata_digest(&scope, &inventory)?;
    let allocation = GenerationAllocation {
        registry_uid: REGISTRY_UID.into(),
        sequence: 1,
        generation_id: initial.generation_id.clone(),
        service_account_name: initial_names.service_account_name,
        transition_id: initial_names.transition_id,
        artifact_digest: initial.artifact_digest.clone(),
        ir_digest: initial.compiled.ir_digest.clone(),
        inventory_metadata_digest,
    };
    let previous = deploy::predecessor_metadata(&initial, &allocation, &claims)?;
    let policy = ceiling(&initial.compiled, &successor.compiled, &target);
    let request = DeploymentRequest {
        transition_id: successor_names.transition_id,
        previous: Some(Box::new(previous)),
        next: successor,
        runtime_ceiling: policy.clone(),
        cleanup_ceiling: policy,
        registry: RegistrySnapshot {
            registry_uid: REGISTRY_UID.into(),
            next_sequence: 2,
            revision: "42".into(),
            claims,
            generation_history: GenerationHistory {
                domain_id: "example".into(),
                cluster_binding: "cluster-a".into(),
                revision: "42".into(),
                active_allocation: Some(allocation),
            },
        },
        observations: vec![],
        ownership_intents: vec![],
    };
    Ok(deploy::plan_transition(&request)?)
}

#[test]
fn purposes_enforce_their_distinct_capability_and_policy_relations() -> TestResult {
    let (cleanup_initial, cleanup_successor) = cleanup_pair("delete")?;
    let cleanup = purpose(&cleanup_initial)?;
    assert!(
        validate_relation(
            &cleanup,
            &cleanup_initial,
            &cleanup_successor,
            INITIAL_DIGEST,
            INITIAL_DIGEST,
        )
        .is_ok()
    );
    assert!(
        validate_relation(
            &cleanup,
            &cleanup_initial,
            &cleanup_successor,
            INITIAL_DIGEST,
            SUCCESSOR_DIGEST,
        )
        .is_err()
    );

    let expansion_initial = cleanup_successor.clone();
    let expansion_successor = cleanup_initial.clone();
    assert!(
        validate_relation(
            &CandidatePairPurpose::PermissionExpansion {},
            &expansion_initial,
            &expansion_successor,
            INITIAL_DIGEST,
            SUCCESSOR_DIGEST,
        )
        .is_ok()
    );
    assert!(
        validate_relation(
            &CandidatePairPurpose::PermissionExpansion {},
            &expansion_initial,
            &expansion_successor,
            INITIAL_DIGEST,
            INITIAL_DIGEST,
        )
        .is_err()
    );
    assert!(
        validate_relation(
            &CandidatePairPurpose::PermissionExpansion {},
            &cleanup_initial,
            &cleanup_successor,
            INITIAL_DIGEST,
            SUCCESSOR_DIGEST,
        )
        .is_err()
    );
    let reversed_cleanup = CandidatePairPurpose::LastTypeCleanup {
        identity: identity(&expansion_successor, "cleanup-target")?,
    };
    assert!(
        validate_relation(
            &reversed_cleanup,
            &expansion_initial,
            &expansion_successor,
            INITIAL_DIGEST,
            INITIAL_DIGEST,
        )
        .is_err()
    );
    assert!(
        validate_relation(
            &CandidatePairPurpose::PermissionExpansion {},
            &cleanup_initial,
            &cleanup_successor,
            "invalid",
            SUCCESSOR_DIGEST,
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn cleanup_rejects_added_or_additionally_removed_resources() -> TestResult {
    let (initial, successor) = cleanup_pair("delete")?;
    let cleanup = purpose(&initial)?;
    let with_extra_removal = compiled(&[
        ResourceFixture {
            id: "cleanup-target",
            kind: "Service",
            deletion_policy: "delete",
        },
        ResourceFixture {
            id: "also-removed",
            kind: "Service",
            deletion_policy: "orphan",
        },
        ResourceFixture {
            id: "common-config",
            kind: "ConfigMap",
            deletion_policy: "orphan",
        },
    ])?;
    assert!(
        validate_relation(
            &cleanup,
            &with_extra_removal,
            &successor,
            INITIAL_DIGEST,
            INITIAL_DIGEST,
        )
        .is_err()
    );

    let with_addition = compiled(&[
        ResourceFixture {
            id: "common-config",
            kind: "ConfigMap",
            deletion_policy: "orphan",
        },
        ResourceFixture {
            id: "new-config",
            kind: "ConfigMap",
            deletion_policy: "orphan",
        },
    ])?;
    assert!(
        validate_relation(
            &cleanup,
            &initial,
            &with_addition,
            INITIAL_DIGEST,
            INITIAL_DIGEST,
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn cleanup_requires_prior_delete_and_complete_api_type_removal() -> TestResult {
    let (orphan_initial, successor) = cleanup_pair("orphan")?;
    assert!(
        validate_relation(
            &purpose(&orphan_initial)?,
            &orphan_initial,
            &successor,
            INITIAL_DIGEST,
            INITIAL_DIGEST,
        )
        .is_err()
    );

    let initial = compiled(&[
        ResourceFixture {
            id: "cleanup-target",
            kind: "Service",
            deletion_policy: "delete",
        },
        ResourceFixture {
            id: "surviving-service",
            kind: "Service",
            deletion_policy: "orphan",
        },
        ResourceFixture {
            id: "common-config",
            kind: "ConfigMap",
            deletion_policy: "orphan",
        },
    ])?;
    let successor = compiled(&[
        ResourceFixture {
            id: "surviving-service",
            kind: "Service",
            deletion_policy: "orphan",
        },
        ResourceFixture {
            id: "common-config",
            kind: "ConfigMap",
            deletion_policy: "orphan",
        },
    ])?;
    assert!(
        validate_relation(
            &purpose(&initial)?,
            &initial,
            &successor,
            INITIAL_DIGEST,
            INITIAL_DIGEST,
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn cleanup_projection_rejects_retained_desired_changes() -> TestResult {
    let (initial, successor) = cleanup_pair("delete")?;
    let cleanup = purpose(&initial)?;
    let mut changed_ir = successor.ir.clone();
    let value = changed_ir
        .resources
        .iter_mut()
        .find(|resource| resource.id == "common-config")
        .and_then(|resource| resource.desired.pointer_mut("/data/value"))
        .ok_or("retained fixture value is absent")?;
    *value = json!("changed");
    let changed = compiler_core::validate(&changed_ir)?;
    assert_eq!(changed.capabilities, successor.capabilities);
    assert!(
        validate_relation(&cleanup, &initial, &changed, INITIAL_DIGEST, INITIAL_DIGEST,).is_err()
    );
    Ok(())
}

#[test]
fn cleanup_projection_rejects_initial_only_input_and_reaction_removal() -> TestResult {
    let (initial, successor) = reactive_cleanup_pair()?;
    assert!(
        successor.capabilities.len() < initial.capabilities.len()
            && successor
                .capabilities
                .iter()
                .all(|capability| initial.capabilities.contains(capability))
    );
    assert!(
        validate_relation(
            &purpose(&initial)?,
            &initial,
            &successor,
            INITIAL_DIGEST,
            INITIAL_DIGEST,
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn cleanup_projection_accepts_source_and_provenance_updates() -> TestResult {
    let (initial, successor) = cleanup_pair("delete")?;
    let cleanup = purpose(&initial)?;
    let mut updated_ir = successor.ir;
    updated_ir.provenance.compiler = "apsides/0.2".into();
    let input = updated_ir
        .provenance
        .inputs
        .first_mut()
        .ok_or("fixture provenance input is absent")?;
    input.path = "successor.ts".into();
    input.sha256 = "1111111111111111111111111111111111111111111111111111111111111111".into();
    let source = &mut updated_ir
        .resources
        .first_mut()
        .ok_or("surviving fixture resource is absent")?
        .source;
    source.file = "successor.ts".into();
    source.line = 17;
    source.column = 4;
    let updated = compiler_core::validate(&updated_ir)?;
    assert!(
        validate_relation(&cleanup, &initial, &updated, INITIAL_DIGEST, INITIAL_DIGEST,).is_ok()
    );
    Ok(())
}

#[test]
fn cleanup_plan_binds_uid_generation_and_exact_named_capabilities() -> TestResult {
    let (initial, successor) = cleanup_pair("delete")?;
    let cleanup = purpose(&initial)?;
    let plan = cleanup_plan(initial, successor)?;
    assert!(validate_cleanup_plan(&cleanup, &plan).is_ok());
    assert!(validate_cleanup_plan(&CandidatePairPurpose::PermissionExpansion {}, &plan).is_ok());

    let mut missing_uid = plan.clone();
    missing_uid.cleanup[0].expected_uid = None;
    assert!(validate_cleanup_plan(&cleanup, &missing_uid).is_err());

    let mut wrong_generation = plan.clone();
    wrong_generation.cleanup[0]
        .prior_generation_id
        .push_str("-changed");
    assert!(validate_cleanup_plan(&cleanup, &wrong_generation).is_err());

    let mut wrong_capability = plan;
    wrong_capability.cleanup_capabilities[0].names = vec!["another-object".into()];
    assert!(validate_cleanup_plan(&cleanup, &wrong_capability).is_err());
    Ok(())
}
