use super::*;
use coordination::{ReviewOwnershipPlan, ReviewPrimitive};

fn projected_from(ir: &Value) -> Result<(deploy::TransitionPlan, coordination::ReviewMetadata)> {
    let (_, plan) = transition(ir)?;
    let metadata = platform::project_review_metadata(&plan)?;
    Ok((plan, metadata))
}

fn projected(private: &str) -> Result<(deploy::TransitionPlan, coordination::ReviewMetadata)> {
    projected_from(&declaration(private)?)
}

fn strictly_sorted<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

fn assert_declaration_catalogs(
    plan: &deploy::TransitionPlan,
    metadata: &coordination::ReviewMetadata,
) -> Result<()> {
    let compiled = &plan.request.next.compiled;
    assert_eq!(metadata.resources.len(), compiled.ir.resources.len());
    for resource in &metadata.resources {
        let source = compiled
            .ir
            .resources
            .iter()
            .find(|candidate| candidate.id == resource.id)
            .ok_or("projected resource lacks compiler declaration")?;
        assert_eq!(resource.source.file, source.source.file);
        assert_eq!(resource.source.line, source.source.line);
        assert_eq!(resource.source.column, source.source.column);
        let effect = compiled
            .effects
            .iter()
            .find(|effect| {
                effect.operation == "server-side-apply"
                    && effect.target == source.id
                    && effect.source == source.source
            })
            .ok_or("projected resource lacks compiler effect")?;
        assert_eq!(resource.apply_fields, effect.field_paths);
    }

    assert_eq!(metadata.inputs.len(), compiled.ir.inputs.len());
    for input in &metadata.inputs {
        let source = compiled
            .ir
            .inputs
            .iter()
            .find(|candidate| candidate.id == input.id)
            .ok_or("projected input lacks compiler declaration")?;
        assert_eq!(input.source.file, source.source.file);
        assert_eq!(input.source.line, source.source.line);
        assert_eq!(input.source.column, source.source.column);
        assert_eq!(input.projection, source.projection);
        assert_eq!(input.sensitive, source.sensitive);
        assert_eq!(input.missing, source.missing);
        assert_eq!(input.managed_resource_id, source.managed_resource_id);
    }

    let reaction = metadata
        .reactions
        .first()
        .ok_or("projected reaction missing")?;
    assert_eq!(reaction.op, ReviewPrimitive::Rollout);
    assert_eq!(reaction.effect_fields, [coordination::REVIEW_ROLLOUT_FIELD]);
    assert_eq!(reaction.source.file, "program.tsx");
    assert_eq!(reaction.source.line, 10);
    Ok(())
}

fn assert_permission_catalogs(
    plan: &deploy::TransitionPlan,
    metadata: &coordination::ReviewMetadata,
) -> Result<()> {
    let compiled = &plan.request.next.compiled;
    let domain = &compiled.ir.domain;
    assert_eq!(
        serde_json::to_value(&metadata.runtime_permissions)?,
        serde_json::to_value(&compiled.capabilities)?
    );
    assert_eq!(
        serde_json::to_value(&metadata.cleanup_permissions)?,
        serde_json::to_value(&plan.cleanup_capabilities)?
    );
    assert!(
        !metadata
            .deployment_permissions
            .required_resources
            .is_empty()
    );
    assert_eq!(
        serde_json::to_value(&metadata.compatibility.runtime_budgets)?,
        serde_json::to_value(&domain.budgets)?
    );
    let required = platform::deployment_permissions(plan)?;
    assert_eq!(
        serde_json::to_value(&metadata.deployment_permissions.required_resources)?,
        serde_json::to_value(&required.required_resources)?
    );
    assert_eq!(
        serde_json::to_value(&metadata.deployment_permissions.creation_targets)?,
        serde_json::to_value(&required.creation_targets)?
    );
    assert_eq!(
        serde_json::to_value(&metadata.enforcement.runtime_ceiling)?,
        serde_json::to_value(&plan.request.runtime_ceiling)?
    );
    assert_eq!(
        serde_json::to_value(&metadata.enforcement.cleanup_ceiling)?,
        serde_json::to_value(&plan.request.cleanup_ceiling)?
    );
    assert_eq!(metadata.enforcement.gaps, compiled.enforcement_gaps);
    assert_eq!(metadata.enforcement.indirect_risks, compiled.indirect_risks);
    Ok(())
}

#[test]
fn real_projector_preserves_complete_compiler_and_permission_catalogs() -> Result<()> {
    let (plan, metadata) = projected("private-projection-canary")?;
    assert_eq!(
        metadata.schema_version,
        coordination::REVIEW_METADATA_SCHEMA
    );
    assert_eq!(
        metadata.scope.domain_id,
        plan.request.next.compiled.ir.domain.id
    );
    assert_eq!(metadata.scope.transition_id, plan.request.transition_id);
    assert_eq!(
        metadata.scope.sequence,
        plan.generation_reservation.sequence
    );
    assert_declaration_catalogs(&plan, &metadata)?;
    assert_permission_catalogs(&plan, &metadata)
}

#[test]
fn private_values_do_not_change_real_projected_metadata() -> Result<()> {
    let first_ir = declaration("private-projection-canary-a")?;
    let mut second_ir = declaration("private-projection-canary-b")?;
    second_ir["provenance"]["inputs"][0]["sha256"] = json!("f".repeat(64));
    let (_, first_plan) = transition(&first_ir)?;
    let (_, second_plan) = transition(&second_ir)?;
    let first = platform::project_review_metadata(&first_plan)?;
    let second = platform::project_review_metadata(&second_plan)?;
    assert_ne!(first_plan.plan_digest, second_plan.plan_digest);
    assert_ne!(
        first_plan.request.next.compiled.ir.provenance.inputs,
        second_plan.request.next.compiled.ir.provenance.inputs
    );
    assert_eq!(first.enforcement.gaps, second.enforcement.gaps);
    assert_eq!(
        first.enforcement.indirect_risks,
        second.enforcement.indirect_risks
    );
    assert_eq!(first, second);
    let encoded = serde_json::to_string(&first)?;
    assert!(!encoded.contains("private-projection-canary"));
    assert!(!encoded.contains("desired"));
    assert!(!encoded.contains("irDigest"));
    Ok(())
}

#[test]
fn projector_normalizes_sets_and_keeps_resource_id_distinct_from_name() -> Result<()> {
    let mut ir = declaration("private-projection-canary")?;
    ir["resources"][0]["id"] = json!("logical-config");
    ir["inputs"][0]["managedResourceId"] = json!("logical-config");
    let (_, plan) = transition(&ir)?;
    let first = platform::project_review_metadata(&plan)?;
    let second = platform::project_review_metadata(&plan)?;
    assert_eq!(first, second);
    assert_eq!(first.reference()?, second.reference()?);

    let config = first
        .resources
        .iter()
        .find(|resource| resource.id == "logical-config")
        .ok_or("logical resource missing")?;
    assert_eq!(config.identity.name, "owned");
    assert_ne!(config.id, config.identity.name);
    assert!(
        first
            .resources
            .windows(2)
            .all(|pair| pair[0].identity < pair[1].identity)
    );
    assert!(strictly_sorted(
        &first
            .inputs
            .iter()
            .map(|input| input.id.as_str())
            .collect::<Vec<_>>()
    ));
    assert_eq!(
        first
            .inputs
            .iter()
            .map(|input| input.id.as_str())
            .collect::<Vec<_>>(),
        ["credentials", "external", "managed"]
    );
    assert_eq!(
        first.reactions.first().ok_or("reaction missing")?.input_ids,
        ["credentials", "external", "managed"]
    );
    assert!(
        first
            .resources
            .iter()
            .all(|resource| strictly_sorted(&resource.apply_fields))
    );
    assert!(strictly_sorted(&first.runtime_permissions));
    assert!(strictly_sorted(&first.cleanup_permissions));
    assert!(strictly_sorted(
        &first.deployment_permissions.required_resources
    ));
    assert!(strictly_sorted(
        &first.deployment_permissions.creation_targets
    ));
    assert!(
        first
            .runtime_permissions
            .iter()
            .chain(&first.cleanup_permissions)
            .all(|capability| strictly_sorted(&capability.names))
    );
    Ok(())
}

#[test]
fn projector_rejects_forged_compiler_catalogs() -> Result<()> {
    let (_, mut effect_forgery) = transition(&declaration("private-projection-canary")?)?;
    effect_forgery
        .request
        .next
        .compiled
        .effects
        .first_mut()
        .ok_or("compiler effect missing")?
        .field_paths
        .push("/forged".to_owned());
    assert!(platform::project_review_metadata(&effect_forgery).is_err());

    let (_, mut permission_forgery) = transition(&declaration("private-projection-canary")?)?;
    permission_forgery
        .request
        .next
        .compiled
        .capabilities
        .pop()
        .ok_or("compiler capability missing")?;
    assert!(platform::project_review_metadata(&permission_forgery).is_err());
    Ok(())
}

#[test]
fn projector_links_the_canonical_adoption_approval_without_private_payloads() -> Result<()> {
    let (_, plan) = super::super::adoption::adoption_fixture()?;
    let metadata = platform::project_review_metadata(&plan)?;
    let adopted = metadata
        .resources
        .iter()
        .find(|resource| resource.identity.name == "adopted")
        .ok_or("adopted resource missing")?;
    assert!(matches!(
        &adopted.ownership_plan,
        ReviewOwnershipPlan::AdoptionApproved {
            intent_object_name,
            object_uid,
            object_resource_version,
            observed_uid,
            observed_resource_version,
        } if intent_object_name == "approve-adopted"
            && object_uid == "approval-object-uid"
            && object_resource_version == "17"
            && observed_uid == "existing-object-uid"
            && observed_resource_version == "23"
    ));
    let encoded = serde_json::to_string(&metadata)?;
    assert!(!encoded.contains("approved-private-value"));
    assert!(!encoded.contains("permittedApply"));
    assert!(!encoded.contains("priorManagedFieldsDigest"));
    Ok(())
}

#[test]
fn projection_fills_handlers_egress_outputs_and_the_surface_digest() -> Result<()> {
    let (plan, metadata) = projected_from(&handler_declaration("private-value-canary")?)?;

    // `surfaceDigest` is filled by the same `derive_surface` function and
    // canonical JSON `aps deploy` will independently recompute, never a
    // second implementation.
    let surface = compiler_core::derive_surface(&plan.request.next.compiled);
    assert_eq!(metadata.surface_digest, compiler_core::digest(&surface)?);

    assert_eq!(metadata.handlers.len(), 1);
    let handler = &metadata.handlers[0];
    assert_eq!(handler.id, "sync-flags");
    assert_eq!(handler.trigger, "settings");
    assert_eq!(handler.inputs, ["settings"]);
    assert_eq!(handler.outputs, ["output-flags"]);

    assert_eq!(metadata.egress.len(), 1);
    let egress = &metadata.egress[0];
    assert_eq!(egress.host, "api.example.com");
    assert_eq!(egress.port, 443);
    assert_eq!(egress.handler, "sync-flags");

    assert_eq!(metadata.outputs.len(), 1);
    let output = &metadata.outputs[0];
    assert_eq!(output.id, "output-flags");
    assert_eq!(output.name, "flags");
    assert_eq!(output.namespace, "workload");
    Ok(())
}
