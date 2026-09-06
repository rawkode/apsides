use super::*;
use coordination::{ReviewCapability, ReviewMetadataV1};
use platform::{
    LivePreviewReport, PreviewCondition, PreviewInterval,
    preview_fields::{PreviewObjects, classify_preview_fields},
};

fn live_report(
    plan: &deploy::TransitionPlan,
    previous: Option<&ReviewMetadataV1>,
) -> Result<LivePreviewReport> {
    let next = platform::project_review_metadata(plan)?;
    let mut resources = Vec::with_capacity(next.resources.len());
    for metadata in &next.resources {
        let declared = plan
            .request
            .next
            .compiled
            .ir
            .resources
            .iter()
            .find(|resource| resource.id == metadata.id)
            .ok_or("preview declaration missing")?;
        resources.push(classify_preview_fields(
            previous,
            &next,
            &metadata.identity,
            PreviewObjects {
                before: previous.map(|_| &declared.desired),
                projected: &declared.desired,
            },
        )?);
    }
    Ok(LivePreviewReport {
        schema_version: "reakt.dev/live-preview/v1",
        plan_digest: plan.plan_digest.clone(),
        interval: PreviewInterval {
            started_at: "2026-09-06T10:00:00Z".into(),
            completed_at: "2026-09-06T10:00:01Z".into(),
        },
        resources,
        conditions: vec![
            PreviewCondition::IssuedDeployerAdmissionOnly,
            PreviewCondition::RuntimeInputsNotRead,
            PreviewCondition::RuntimeConditionalTargetsNotPredicted,
            PreviewCondition::ObservationIntervalNotAtomic,
            PreviewCondition::LifecycleDeletesNotPreviewed,
            PreviewCondition::NoExecutionAuthority,
        ],
    })
}

fn render_live(private: &str) -> Result<Value> {
    let (stored, plan) = transition(&declaration(private)?)?;
    let live = crate::review::LiveReview::fresh(&plan, None, live_report(&plan, None)?)?;
    crate::review::preflight_with_live(&plan, None, &live)?;
    crate::review::transition_review_fixture_with_live(&stored, &plan, TRANSITION_1, None, &live)
}

#[test]
fn v5_renders_bounded_live_evidence_and_supplemental_permissions() -> Result<()> {
    let report = render_live("private-live-canary")?;
    assert_eq!(
        report.pointer("/schemaVersion"),
        Some(&json!("reakt.dev/deployment-review/v5"))
    );
    assert_eq!(
        report.pointer("/liveImpact/status"),
        Some(&json!("available"))
    );
    assert_eq!(
        report.pointer("/liveImpact/observationContext"),
        Some(&json!("fresh-pre-reservation"))
    );
    assert_eq!(
        report.pointer("/liveImpact/report/schemaVersion"),
        Some(&json!("reakt.dev/live-preview/v1"))
    );
    assert_eq!(
        report.pointer("/liveImpact/supplementalPermissions/scope"),
        Some(&json!("supplemental-live-preview-only"))
    );
    let targets = report
        .pointer("/liveImpact/supplementalPermissions/targetRequests")
        .and_then(Value::as_array)
        .ok_or("preview permission targets missing")?;
    assert_eq!(targets.len(), 2);
    assert!(targets.iter().all(|target| {
        target.get("listMethod") == Some(&json!("LIST"))
            && target.get("patchMethod") == Some(&json!("PATCH"))
            && target.get("patchDryRun") == Some(&json!("All"))
    }));
    let encoded = serde_json::to_string(&report)?;
    assert!(!encoded.contains("private-live-canary"));
    assert!(!encoded.contains(&"0".repeat(64)));
    assert!(!encoded.contains("irDigest"));
    assert!(!encoded.contains("permittedApply"));
    Ok(())
}

#[test]
fn changed_plan_digest_or_predecessor_reference_is_rejected() -> Result<()> {
    let ir = declaration("private-binding-canary")?;
    let (_, initial) = transition(&ir)?;
    let mut wrong_digest = live_report(&initial, None)?;
    wrong_digest.plan_digest = format!("sha256:{}", "f".repeat(64));
    assert!(crate::review::LiveReview::fresh(&initial, None, wrong_digest).is_err());

    let (_, upgrade, previous) = super::v4::upgrade(&ir, &ir)?;
    let live = crate::review::LiveReview::fresh(
        &upgrade,
        Some(&previous),
        live_report(&upgrade, Some(&previous))?,
    )?;
    let next = platform::project_review_metadata(&upgrade)?;
    assert!(crate::review::preflight_with_live(&upgrade, Some(&next), &live).is_err());
    let mut substituted = previous;
    substituted
        .resources
        .first_mut()
        .ok_or("predecessor resource missing")?
        .source
        .line += 1;
    substituted.validate()?;
    assert!(crate::review::preflight_with_live(&upgrade, Some(&substituted), &live).is_err());
    Ok(())
}

#[test]
fn v5_opt_out_is_explicitly_unavailable() -> Result<()> {
    let (stored, plan) = transition(&declaration("private-opt-out-canary")?)?;
    let live = crate::review::LiveReview::Unavailable {
        reason: "deployment-identity-preview-not-approved",
    };
    let report = crate::review::transition_review_fixture_with_live(
        &stored,
        &plan,
        TRANSITION_1,
        None,
        &live,
    )?;
    assert_eq!(
        report.pointer("/schemaVersion"),
        Some(&json!("reakt.dev/deployment-review/v5"))
    );
    assert_eq!(
        report.pointer("/liveImpact"),
        Some(&json!({
            "status":"unavailable",
            "reason":"deployment-identity-preview-not-approved"
        }))
    );
    Ok(())
}

fn amplified_previous(base: &ReviewMetadataV1, names: &[String], count: usize) -> ReviewMetadataV1 {
    let mut previous = base.clone();
    if count > 0 {
        let mut capabilities = previous.enforcement.runtime_ceiling.capabilities.clone();
        capabilities.push(ReviewCapability {
            api_group: String::new(),
            resource: "configmaps".into(),
            subresource: None,
            namespace: "workload".into(),
            verb: "get".into(),
            names: names.iter().take(count).cloned().collect(),
        });
        capabilities.sort();
        previous.enforcement.runtime_ceiling.capabilities = capabilities.clone();
        previous.enforcement.cleanup_ceiling.capabilities = capabilities;
    }
    previous
}

#[test]
fn one_global_budget_covers_declaration_and_live_evidence() -> Result<()> {
    let ir = declaration("private-global-budget-canary")?;
    let (_, plan, base) = super::v4::upgrade(&ir, &ir)?;
    let names: Vec<_> = (0..23_000)
        .map(|index| {
            format!(
                "n{index:05}.{}.{}.{}",
                "a".repeat(50),
                "b".repeat(50),
                "c".repeat(50)
            )
        })
        .collect();
    let mut passing = 0usize;
    let mut failing = names.len();
    assert!(
        crate::review::preflight(&plan, Some(&amplified_previous(&base, &names, failing))).is_err()
    );
    while passing + 1 < failing {
        let candidate = passing + (failing - passing) / 2;
        let previous = amplified_previous(&base, &names, candidate);
        if crate::review::preflight(&plan, Some(&previous)).is_ok() {
            passing = candidate;
        } else {
            failing = candidate;
        }
    }
    let previous = amplified_previous(&base, &names, passing);
    crate::review::preflight(&plan, Some(&previous))?;
    let preview = live_report(&plan, Some(&previous))?;
    assert!(serde_json::to_vec(&preview)?.len() < 8 * 1024 * 1024);
    let live = crate::review::LiveReview::fresh(&plan, Some(&previous), preview)?;
    let Err(error) = crate::review::preflight_with_live(&plan, Some(&previous), &live) else {
        return Err("separately bounded live evidence exceeded the global review budget".into());
    };
    assert!(
        error
            .to_string()
            .contains("metadata projection limit exceeded")
    );
    assert!(!error.to_string().contains("private-global-budget-canary"));
    Ok(())
}
