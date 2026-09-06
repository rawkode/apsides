//! Same pending kernel and real selected-session replay; only the issued client
//! and verified-release boundary use crate-private unit-test fixtures.
use super::super::tests::{NoInventory, empty_list, identity, state, version};
use super::*;
use crate::Versioned;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;
type Calls = Arc<Mutex<Vec<(String, String, Value)>>>;
struct Fixture {
    issued: IssuedDeployment,
    plan: deploy::TransitionPlan,
    trust_digest: String,
    calls: Calls,
}

#[derive(Clone, Copy)]
enum Mode {
    UidUnbound,
    UidKnown,
    Replaced,
    FinalDrift,
}

fn fixture(mode: Mode) -> TestResult<Fixture> {
    let (mut registry, plan, policy, trust) = crate::tests::preview_pending_fixture()?;
    // Seed the immutable pre-reservation empty claims used by actual bind_release.
    let (_, fresh, _, _) = crate::tests::preview_fresh_fixture()?;
    crate::tests::registry_object(&fresh, &plan.request.registry.registry_uid, "42")?;
    let stored = registry
        .transitions
        .get_mut(&plan.request.transition_id)
        .ok_or("fixture transition missing")?;
    stored.record = deploy::advance(
        &plan,
        &stored.record,
        stored.record.revision,
        "executor-a",
        deploy::Event::Quiesced {
            event_id: "preview-quiesced".into(),
            evidence: deploy::Quiescence::InitialInstallation {
                verification_epoch: stored.record.verification_epoch,
            },
        },
    )?;
    let claim = registry.claims.first_mut().ok_or("fixture claim missing")?;
    claim.state = deploy::ClaimState::Active; // Actual crash state: claims active, record still Quiesced.
    if !matches!(mode, Mode::UidUnbound) {
        claim.uid = Some("target-uid".into());
    }
    let review = crate::project_review_metadata(&plan)?;
    let identity = identity(
        review
            .resources
            .iter()
            .map(|resource| resource.identity.clone())
            .collect(),
    );
    let identity_object = state(
        "reakt-deploy-identity-example",
        &identity,
        "identity-uid",
        "1",
    )?;
    bind_preview_executor(&mut registry, &plan, &identity)?;
    let registry_object =
        crate::tests::registry_object(&registry, &plan.request.registry.registry_uid, "43")?;
    let policy_object = state("reakt-policy-example", &policy, "policy-uid", "40")?;
    let trust_object = state("reakt-trust-example", &trust, "trust-uid", "43")?;
    let resolve = vec![
        registry_object.clone(),
        trust_object.clone(),
        policy_object.clone(),
    ];
    let mut group = vec![identity_object.clone()];
    group.extend(resolve.clone());
    group.extend([policy_object.clone(), trust_object.clone()]);
    let mut replies = resolve.clone(); // bind_release
    replies.extend(resolve.clone()); // PendingContext initial resolve
    replies.extend([
        identity_object.clone(),
        version(),
        policy_object,
        trust_object,
    ]);
    replies.extend(group.clone()); // authentication recheck
    replies.extend(group.clone()); // pre-capture recheck
    replies.push(target_list(&plan, matches!(mode, Mode::Replaced)));
    replies.extend(group.clone()); // post-capture recheck
    if !matches!(mode, Mode::UidUnbound | Mode::Replaced) {
        replies.extend(group.clone()); // pre-apply recheck
        replies.push(target_list(&plan, false)); // exact re-list before PATCH
        replies.push(target_object(&plan, false));
        replies.extend(group.clone()); // post-apply recheck
    }
    replies.extend(group.clone()); // final pre-target recheck
    replies.push(if matches!(mode, Mode::FinalDrift) {
        empty_list()
    } else {
        target_list(&plan, false)
    });
    replies.extend(group); // final authority recheck
    replies.extend(resolve); // test's final authenticated resolve
    replies.retain(|reply| reply["metadata"]["name"] != "reakt-deploy-identity-example");
    let (client, calls) = crate::tests::wire_with_identity(replies, identity_object);
    let platform = crate::tests::platform_client(client, "control", "workload", "cluster-a")?;
    let issued = IssuedDeployment::for_preview_test(
        platform,
        Versioned {
            value: identity,
            uid: "identity-uid".into(),
            resource_version: "1".into(),
        },
    )?;
    Ok(Fixture {
        issued,
        plan,
        trust_digest: compiler_core::digest(&trust)?,
        calls,
    })
}
fn bind_preview_executor(
    registry: &mut crate::NamespaceRegistry,
    plan: &deploy::TransitionPlan,
    identity: &crate::DeploymentIdentity,
) -> TestResult {
    let stored = registry
        .transitions
        .get_mut(&plan.request.transition_id)
        .ok_or("fixture transition missing")?;
    let binding = &mut stored.executor_binding;
    binding.identity_approval_uid = "identity-uid".into();
    binding.identity_approval_resource_version = "1".into();
    binding.identity_approval_digest = compiler_core::digest(identity)?;
    binding.service_account_name = identity.service_account_name.clone();
    binding.service_account_uid = identity.service_account_uid.clone();
    binding.binding_secret_name = identity.binding_secret_name.clone();
    binding.binding_secret_uid = identity.binding_secret_uid.clone();
    binding.audience = identity.audience.clone();
    let profile = stored.capacity.profile.clone();
    let capacity = crate::completion_envelope::calculate(registry, plan, &profile)?;
    registry
        .transitions
        .get_mut(&plan.request.transition_id)
        .ok_or("fixture transition missing")?
        .capacity = capacity;
    Ok(())
}

fn target_object(plan: &deploy::TransitionPlan, replaced: bool) -> Value {
    let mut value = plan.request.next.compiled.ir.resources[0].desired.clone();
    value["metadata"]["uid"] = json!(if replaced {
        "replacement-uid"
    } else {
        "target-uid"
    });
    value["metadata"]["resourceVersion"] = json!("90");
    value["metadata"]["managedFields"] = json!([{"manager":"reakt-example","operation":"Apply","apiVersion":"v1",
        "fieldsType":"FieldsV1","fieldsV1":{"f:data":{"f:value":{}}}}]);
    value
}
fn target_list(plan: &deploy::TransitionPlan, replaced: bool) -> Value {
    json!({"apiVersion":"v1","kind":"ConfigMapList","metadata":{"resourceVersion":"91"},"items":[target_object(plan,replaced)]})
}
async fn session(fixture: &Fixture) -> TestResult<TransitionSession> {
    Ok(Box::pin(fixture.issued.platform().bind_release(
        &fixture.plan.request.next.compiled,
        &fixture.plan.request.next.artifact_digest,
        &fixture.trust_digest,
        &NoInventory,
        &fixture.plan.request.transition_id,
    ))
    .await?)
}

#[tokio::test]
async fn active_claim_without_uid_is_whole_unavailable_with_no_patch() -> TestResult {
    let fixture = fixture(Mode::UidUnbound)?;
    let session = session(&fixture).await?;
    let (outcome, binding) = Box::pin(preview_selected(&fixture.issued, &session)).await?;
    assert!(matches!(
        outcome,
        PendingPreviewOutcome::Unavailable(PendingPreviewUnavailable::TargetUidNotProven)
    ));
    assert_eq!(binding.phase, deploy::Phase::Quiesced);
    let resolved = session.resolve().await?;
    binding.validate(&resolved)?;
    let calls = fixture.calls.lock().map_err(|_| "call lock")?;
    assert!(calls.iter().all(|(method, _, _)| method == "GET"));
    assert_eq!(
        calls
            .iter()
            .filter(|(_, path, _)| path.contains("/namespaces/workload/"))
            .count(),
        2
    );
    Ok(())
}

#[tokio::test]
async fn current_active_uid_authorizes_exact_existing_preview_and_binding_is_pinned() -> TestResult
{
    let fixture = fixture(Mode::UidKnown)?;
    assert!(fixture.plan.request.registry.claims.is_empty()); // Frozen request cannot supply this UID.
    let session = session(&fixture).await?;
    let (outcome, binding) = Box::pin(preview_selected(&fixture.issued, &session)).await?;
    assert!(matches!(outcome, PendingPreviewOutcome::Available(_)));
    let resolved = session.resolve().await?;
    binding.validate(&resolved)?;
    let mut changed = binding.clone();
    changed.record_revision += 1;
    assert!(changed.validate(&resolved).is_err());
    changed = binding.clone();
    changed.registry_resource_version = "changed".into();
    assert!(changed.validate(&resolved).is_err());
    changed = binding.clone();
    changed.next_review_metadata.digest = compiler_core::digest(&"substitution")?;
    assert!(changed.validate(&resolved).is_err());
    let calls = fixture.calls.lock().map_err(|_| "call lock")?;
    let patches: Vec<_> = calls
        .iter()
        .filter(|(method, _, _)| method == "PATCH")
        .collect();
    assert_eq!(patches.len(), 1);
    assert!(patches[0].1.contains("dryRun=All"));
    assert_eq!(patches[0].2["metadata"]["uid"], json!("target-uid"));
    assert_eq!(patches[0].2["metadata"]["resourceVersion"], json!("90"));
    assert_eq!(
        calls
            .iter()
            .filter(|(method, path, _)| method == "GET" && path.contains("/namespaces/workload/"))
            .count(),
        3
    );
    Ok(())
}

#[tokio::test]
async fn target_replacement_is_error_and_final_drift_is_not_unavailable() -> TestResult {
    for mode in [Mode::Replaced, Mode::FinalDrift] {
        let fixture = fixture(mode)?;
        let session = session(&fixture).await?;
        let error = Box::pin(preview_selected(&fixture.issued, &session))
            .await
            .err()
            .ok_or("pending target drift was accepted")?;
        let expected = if matches!(mode, Mode::Replaced) {
            "pending target UID was replaced"
        } else {
            "preview target changed during the observation interval"
        };
        assert!(
            error.to_string().contains(expected),
            "unexpected rejection: {error}"
        );
        let calls = fixture.calls.lock().map_err(|_| "call lock")?;
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _, _)| method == "PATCH")
                .count(),
            usize::from(matches!(mode, Mode::FinalDrift))
        );
    }
    Ok(())
}
