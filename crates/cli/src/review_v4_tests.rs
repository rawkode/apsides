use super::*;
use coordination::ReviewMetadataV1;

fn upgrade(
    before: &Value,
    after: &Value,
) -> Result<(StoredTransition, deploy::TransitionPlan, ReviewMetadataV1)> {
    upgrade_with_ceiling(before, after, 0)
}

fn add_ceiling_names(request: &mut deploy::DeploymentRequest, count: usize) {
    if count == 0 {
        return;
    }
    let names: Vec<_> = (0..count).map(|index| format!("n{index:05}")).collect();
    let capability = compiler_core::Capability {
        api_group: String::new(),
        resource: "configmaps".into(),
        subresource: None,
        namespace: "workload".into(),
        verb: "get".into(),
        names,
    };
    request
        .runtime_ceiling
        .capabilities
        .push(capability.clone());
    request.cleanup_ceiling.capabilities.push(capability);
}

fn upgrade_with_ceiling(
    before: &Value,
    after: &Value,
    count: usize,
) -> Result<(StoredTransition, deploy::TransitionPlan, ReviewMetadataV1)> {
    let (mut stored, mut old) = transition(before)?;
    add_ceiling_names(&mut old.request, count);
    old = plan_transition(&old.request)?;
    let previous = platform::project_review_metadata(&old)?;
    let mut request = old.request.clone();
    request.transition_id = TRANSITION_2.into();
    request.next.generation_id = GENERATION_2.into();
    request.next.compiled = compiler_core::parse_and_validate(&after.to_string())?;
    request.registry.next_sequence = 2;
    request.registry.revision = "2".into();
    request.registry.generation_history.revision = "2".into();
    request.registry.generation_history.active_allocation =
        Some(old.generation_reservation.clone());
    request.registry.claims = previous
        .resources
        .iter()
        .map(|resource| deploy::ProtectedClaim {
            identity: deploy::ResourceIdentity {
                api_group: resource.identity.api_group.clone(),
                resource: resource.identity.resource.clone(),
                namespace: resource.identity.namespace.clone(),
                name: resource.identity.name.clone(),
            },
            domain_id: previous.scope.domain_id.clone(),
            generation_id: old.generation_reservation.generation_id.clone(),
            transition_id: old.request.transition_id.clone(),
            state: deploy::ClaimState::Active,
            uid: Some(format!("uid-{}", resource.id)),
        })
        .collect();
    request.previous = Some(deploy::PreviousGeneration::Metadata(Box::new(
        deploy::predecessor_metadata(
            &old.request.next,
            &old.generation_reservation,
            &request.registry.claims,
        )?,
    )));
    let next = &request.next.compiled;
    request.runtime_ceiling.capabilities = next.capabilities.clone();
    request.runtime_ceiling.accepted_enforcement_gaps = next.enforcement_gaps.clone();
    request.runtime_ceiling.accepted_indirect_risks = next.indirect_risks.clone();
    request.cleanup_ceiling = request.runtime_ceiling.clone();
    for resource in &previous.resources {
        if resource.lifecycle.deletion_policy == coordination::ReviewDeletionPolicy::Delete
            && !request.next.compiled.ir.resources.iter().any(|next| {
                next.name == resource.identity.name
                    && next.namespace == resource.identity.namespace
                    && next.kind == resource.kind
            })
        {
            for verb in ["get", "delete"] {
                request
                    .cleanup_ceiling
                    .capabilities
                    .push(compiler_core::Capability {
                        api_group: resource.identity.api_group.clone(),
                        resource: resource.identity.resource.clone(),
                        subresource: None,
                        namespace: resource.identity.namespace.clone(),
                        verb: verb.into(),
                        names: vec![resource.identity.name.clone()],
                    });
            }
        }
    }
    add_ceiling_names(&mut request, count);
    let plan = plan_transition(&request)?;
    stored.inputs = frozen_inputs(&plan)?;
    stored.record = begin(&plan, "job-2")?;
    stored.next_review_metadata = platform::project_review_metadata(&plan)?.reference()?;
    stored.previous_review_metadata = Some(previous.reference()?);
    stored.previous_writer_checkpoint = Some(review_checkpoint(
        plan.request.predecessor()?.ok_or("missing predecessor")?,
    )?);
    Ok((stored, plan, previous))
}

fn render(before: &Value, after: &Value) -> Result<Value> {
    let (stored, plan, previous) = upgrade(before, after)?;
    crate::review::preflight(&plan, Some(&previous))?;
    crate::review::transition_review_fixture(&stored, &plan, TRANSITION_2, Some(&previous))
}

fn config(id: &str) -> Value {
    json!({"id":id,"apiVersion":"v1","kind":"ConfigMap","name":id,"namespace":"workload",
      "desired":{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":id,"namespace":"workload"},"data":{"value":"private-canary"}},
      "lifecycle":{"deletionPolicy":"orphan"},"source":source(30)})
}

#[test]
fn upgrade_reports_authenticated_resource_graph_source_and_permission_deltas() -> Result<()> {
    let mut before = declaration("private-before")?;
    before["resources"]
        .as_array_mut()
        .ok_or("resources missing")?
        .push(config("gone"));
    let mut after = declaration("private-after")?;
    after["resources"]
        .as_array_mut()
        .ok_or("resources missing")?
        .push(config("added"));
    after["resources"][0]["desired"]["data"] =
        json!({"private":"private-after", "new":"private-after"});
    after["resources"][0]["source"] = source(40);
    after["resources"][0]["force"] = json!(true);
    after["resources"][0]["lifecycle"]["deletionPolicy"] = json!("delete");
    after["inputs"][1]["id"] = json!("new-external");
    after["inputs"][1]["name"] = json!("new-external");
    after["reactions"][0]["inputs"] = json!(["managed", "new-external", "credentials"]);
    after["reactions"][0]["source"] = source(50);
    let report = render(&before, &after)?;
    assert_eq!(report["schemaVersion"], "reakt.dev/deployment-review/v4");
    assert_eq!(
        report["detailAvailability"],
        "authenticated-declaration-metadata"
    );
    let resources = report["resources"].as_array().ok_or("resources missing")?;
    let row = |name| {
        resources
            .iter()
            .find(|row| row["name"] == name)
            .ok_or("resource missing")
    };
    assert_eq!(row("gone")?["declarationChange"], "removed");
    assert_eq!(row("added")?["declarationChange"], "added");
    assert_eq!(row("api")?["declarationChange"], "unchanged");
    assert_eq!(row("owned")?["declarationChange"], "changed");
    assert_eq!(row("owned")?["beforeForce"], false);
    assert_eq!(row("owned")?["afterForce"], true);
    assert_eq!(row("owned")?["beforeLifecycle"]["deletionPolicy"], "orphan");
    assert_eq!(row("owned")?["afterLifecycle"]["deletionPolicy"], "delete");
    assert_eq!(
        row("owned")?["fieldChanges"],
        json!([
            {"path":"/data/a~1b~0c","change":"removed"},
            {"path":"/data/new","change":"added"},
            {"path":"/data/private","change":"retained"}
        ])
    );
    assert_eq!(report["inputAuthority"]["removed"][0]["id"], "external");
    assert_eq!(report["inputAuthority"]["added"][0]["id"], "new-external");
    assert_eq!(report["reactions"]["changed"][0]["before"]["id"], "restart");
    assert_eq!(report["reactions"]["changed"][0]["after"]["id"], "restart");
    assert!(
        !report["effects"]["changed"]
            .as_array()
            .ok_or("effects missing")?
            .is_empty()
    );
    assert!(
        !report["dataflow"]["changed"]
            .as_array()
            .ok_or("dataflow missing")?
            .is_empty()
    );
    assert!(
        !report["sourceLocations"]["changed"]
            .as_array()
            .ok_or("sources missing")?
            .is_empty()
    );
    assert!(
        !report["permissions"]["runtime"]["added"]
            .as_array()
            .ok_or("permissions missing")?
            .is_empty()
    );
    assert!(
        !report["permissions"]["runtime"]["removed"]
            .as_array()
            .ok_or("permissions missing")?
            .is_empty()
    );
    assert_eq!(report["liveImpact"]["status"], "unavailable");
    assert_eq!(report["actionSemantics"], "planned");
    let encoded = report.to_string();
    for private in ["private-before", "private-after", "private-canary"] {
        assert!(!encoded.contains(private));
    }
    Ok(())
}

#[test]
fn an_empty_authenticated_category_is_still_an_upgrade_baseline() -> Result<()> {
    let mut before = declaration("private")?;
    let mut removed = config("gone");
    removed["lifecycle"]["deletionPolicy"] = json!("delete");
    before["resources"] = json!([removed]);
    before["inputs"] = json!([]);
    before["reactions"] = json!([]);
    let report = render(&before, &declaration("private")?)?;
    for category in [
        &report["reactions"],
        &report["permissions"]["cleanup"]["changes"],
    ] {
        assert_eq!(category["baseline"], "authenticated-declaration-metadata");
        assert_eq!(category["before"], json!([]));
        assert_eq!(category["added"], category["after"]);
    }
    Ok(())
}

#[test]
fn private_only_upgrade_changes_preserve_declarations_and_reject_swapped_metadata() -> Result<()> {
    let before = declaration("private-before")?;
    let after = declaration("private-after")?;
    let (stored, plan, previous) = upgrade(&before, &after)?;
    let report =
        crate::review::transition_review_fixture(&stored, &plan, TRANSITION_2, Some(&previous))?;
    assert!(
        report["resources"]
            .as_array()
            .ok_or("resources missing")?
            .iter()
            .all(|r| r["declarationChange"] == "unchanged")
    );
    for category in [
        "reactions",
        "inputAuthority",
        "effects",
        "dataflow",
        "sourceLocations",
    ] {
        assert_eq!(report[category]["changed"], json!([]), "{category}");
        assert_eq!(report[category]["added"], json!([]), "{category}");
        assert_eq!(report[category]["removed"], json!([]), "{category}");
    }
    assert!(crate::review::preflight(&plan, None).is_err());
    let mut swapped = previous.clone();
    swapped.resources[0].source.line += 1;
    swapped.validate()?;
    assert!(
        crate::review::transition_review_fixture(&stored, &plan, TRANSITION_2, Some(&swapped))
            .is_err()
    );
    let mut wrong_scope = previous;
    wrong_scope.scope.domain_id = "foreign".into();
    assert!(crate::review::preflight(&plan, Some(&wrong_scope)).is_err());
    Ok(())
}

#[test]
fn combined_upgrade_catalog_budget_is_enforced() -> Result<()> {
    let ir = declaration("private-budget-canary")?;
    let (_, plan, previous) = upgrade_with_ceiling(&ir, &ir, 26_000)?;
    previous.validate()?;
    platform::project_review_metadata(&plan)?.validate()?;
    let Err(error) = crate::review::preflight(&plan, Some(&previous)) else {
        return Err("oversized combined catalog accepted".into());
    };
    assert!(
        error
            .to_string()
            .contains("metadata projection limit exceeded")
    );
    assert!(!error.to_string().contains("private-budget-canary"));
    Ok(())
}

#[test]
fn policy_risk_compatibility_and_complete_deployment_contract_changes_are_visible() -> Result<()> {
    let before = declaration("private")?;
    let mut after = before.clone();
    after["resources"][1]["desired"]["spec"]["template"]["spec"]["containers"][0]["image"] =
        json!("nginx:stable");
    after["domain"]["budgets"]["maxQueueDepth"] = json!(64);
    let (stored, plan, previous) = upgrade(&before, &after)?;
    let next = platform::project_review_metadata(&plan)?;
    assert_ne!(
        previous.enforcement.indirect_risks,
        next.enforcement.indirect_risks
    );
    let report =
        crate::review::transition_review_fixture(&stored, &plan, TRANSITION_2, Some(&previous))?;
    assert_eq!(
        report["enforcement"]["before"],
        json!([previous.enforcement])
    );
    assert_eq!(report["enforcement"]["after"], json!([next.enforcement]));
    assert_eq!(
        report["enforcement"]["changed"],
        json!([{ "before": previous.enforcement, "after": next.enforcement }])
    );
    assert_eq!(
        report["compatibility"]["changed"],
        json!([{ "before": previous.compatibility, "after": next.compatibility }])
    );
    assert_eq!(
        report["permissions"]["deployment"]["changes"]["before"],
        json!([previous.deployment_permissions])
    );
    assert_eq!(
        report["permissions"]["deployment"]["changes"]["after"],
        json!([next.deployment_permissions])
    );
    assert_eq!(
        report["permissions"]["deployment"]["roleDelegation"]["before"],
        json!(previous.deployment_permissions.role_delegation)
    );
    assert_eq!(
        report["permissions"]["deployment"]["roleDelegation"]["after"],
        json!(next.deployment_permissions.role_delegation)
    );
    Ok(())
}

#[test]
fn valid_historical_metadata_cannot_amplify_a_report_past_its_byte_budget() -> Result<()> {
    let ir = declaration("private-budget-canary")?;
    let (_, plan, mut previous) = upgrade(&ir, &ir)?;
    let mut capabilities = previous.enforcement.runtime_ceiling.capabilities.clone();
    capabilities.push(coordination::ReviewCapability {
        api_group: String::new(),
        resource: "configmaps".into(),
        subresource: None,
        namespace: "workload".into(),
        verb: "get".into(),
        names: (0..23_000)
            .map(|index| {
                format!(
                    "n{index:05}.{}.{}.{}",
                    "a".repeat(50),
                    "b".repeat(50),
                    "c".repeat(50)
                )
            })
            .collect(),
    });
    capabilities.sort();
    previous.enforcement.runtime_ceiling.capabilities = capabilities.clone();
    previous.enforcement.cleanup_ceiling.capabilities = capabilities;
    // Exercise the accepted historical snapshot contract, independently of the
    // current compiler's smaller request limit; no old release is available here.
    previous.validate()?;
    let Err(error) = crate::review::preflight(&plan, Some(&previous)) else {
        return Err("oversized serialized upgrade report accepted".into());
    };
    assert!(
        error
            .to_string()
            .contains("metadata projection limit exceeded")
    );
    Ok(())
}

#[test]
fn unsupported_historical_deployment_limitations_are_rejected() -> Result<()> {
    let ir = declaration("private")?;
    let (_, plan, mut previous) = upgrade(&ir, &ir)?;
    previous.deployment_permissions.limitations.pop();
    assert!(crate::review::preflight(&plan, Some(&previous)).is_err());
    Ok(())
}

#[test]
fn authenticated_review_and_inventory_policy_must_agree() -> Result<()> {
    let ir = declaration("private")?;
    let (mut stored, plan, mut previous) = upgrade(&ir, &ir)?;
    previous.resources[0].lifecycle.deletion_policy = coordination::ReviewDeletionPolicy::Delete;
    previous.validate()?;
    stored.previous_review_metadata = Some(previous.reference()?);
    assert!(
        crate::review::transition_review_fixture(&stored, &plan, TRANSITION_2, Some(&previous))
            .is_err()
    );
    Ok(())
}
