//! Independent primitive invariants over synthetic data. API/workload cases
//! are reported as unsupported here; they need a cluster to run.
use crate::{
    BlockReason, CompiledPlan, Error, InputSnapshot, Reaction, Result, RolloutKey, RolloutOutcome,
    TestCase, digest, validate,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaseStatus {
    Passed,
    Failed,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CaseResult {
    pub case: TestCase,
    pub status: CaseStatus,
    pub fixture_digest: String,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SemanticReport {
    pub schema_version: String,
    pub ir_digest: String,
    pub primitive_contract: String,
    pub test_only: bool,
    pub production_release_ready: bool,
    pub results: Vec<CaseResult>,
}

fn snapshots(plan: &CompiledPlan) -> BTreeMap<String, InputSnapshot> {
    plan.ir
        .inputs
        .iter()
        .map(|input| {
            let data = if input.kind == "Secret" {
                json!({"fixture":"c3ludGhldGljLW9ubHk="})
            } else {
                json!({"fixture":"synthetic-only"})
            };
            (
                input.id.clone(),
                InputSnapshot::Available {
                    data,
                    authoritative_applied: true,
                },
            )
        })
        .collect()
}

fn ready(outcome: &RolloutOutcome) -> Option<&str> {
    match outcome {
        RolloutOutcome::Ready { revision } => Some(revision),
        RolloutOutcome::Blocked { .. } => None,
    }
}

/// Runs only semantic cases. Every required higher-layer case is reported as
/// Unsupported, so a plan with cluster cases never reports a full pass here.
///
/// # Errors
/// Rejects forged plans or invalid synthetic scenario evaluation.
///
pub fn run_semantic_tests(plan: &CompiledPlan) -> Result<SemanticReport> {
    if validate(&plan.ir)? != *plan {
        return Err(Error::Invalid(
            "semantic runner received an untrusted plan".into(),
        ));
    }
    let key = RolloutKey {
        id: "synthetic-v1".into(),
        bytes: vec![0x5a; 32],
    };
    let fixture_digest = digest(
        &json!({"contract":"apsides.dev/synthetic-fixture/v1","seed":0,"irDigest":plan.ir_digest}),
    )?;
    let mut results = Vec::new();
    for case in &plan.test_plan {
        if case.layer != "semantic" {
            results.push(CaseResult {
                case: case.clone(),
                status: CaseStatus::Unsupported,
                fixture_digest: fixture_digest.clone(),
                detail: "requires a disposable Kubernetes integration runner; not executed by the semantic runner".into(),
            });
            continue;
        }
        let reaction = plan
            .ir
            .reactions
            .iter()
            .find(|reaction| reaction.id == case.reaction_id)
            .ok_or_else(|| Error::Invalid("case has unresolved reaction".into()))?;
        let passed = run_case(plan, case, reaction, &key)?;
        let input_ids = case.input_ids.join(",");
        results.push(CaseResult {
            case: case.clone(),
            status: if passed {
                CaseStatus::Passed
            } else {
                CaseStatus::Failed
            },
            fixture_digest: fixture_digest.clone(),
            detail: if passed {
                format!(
                    "primitive invariant passed for declared inputs [{input_ids}] using synthetic projections; this is not API authorization evidence"
                )
            } else {
                format!(
                    "primitive invariant failed for declared inputs [{input_ids}]; sensitive projections omitted"
                )
            },
        });
    }
    Ok(SemanticReport {
        schema_version: "apsides.dev/semantic-results/v1".into(),
        ir_digest: plan.ir_digest.clone(),
        primitive_contract: "apsides.dev/rollout/v1".into(),
        test_only: true,
        production_release_ready: false,
        results,
    })
}

fn run_case(
    plan: &CompiledPlan,
    case: &TestCase,
    reaction: &Reaction,
    key: &RolloutKey,
) -> Result<bool> {
    let baseline_snapshots = snapshots(plan);
    let baseline = crate::rollout::evaluate_validated_rollout(
        plan,
        &reaction.target,
        &baseline_snapshots,
        Some(key),
    )?;
    let evaluate = |snapshots: &BTreeMap<String, InputSnapshot>| {
        crate::rollout::evaluate_validated_rollout(plan, &reaction.target, snapshots, Some(key))
    };
    let passed = match case.scenario.as_str() {
        "initial-evaluation" => ready(&baseline).is_some(),
        "same-value-replay" | "irrelevant-change" => {
            // Only the typed data projection is passed to evaluation.
            // Metadata changes cannot enter this primitive's vocabulary.
            let replay = evaluate(&snapshots(plan))?;
            ready(&baseline).is_some() && replay == baseline
        }
        "input-returned" => {
            every_input_blocks(plan, reaction, &case.input_ids, key, &BlockReason::Missing)?
                && ready(&baseline).is_some()
                && evaluate(&snapshots(plan))? == baseline
        }
        "projected-change" => every_input_changes(plan, reaction, &case.input_ids, key, &baseline)?,
        "coalesced-inputs" => {
            coalesced_inputs_use_latest_set(plan, reaction, &case.input_ids, key, &baseline)?
        }
        scenario => {
            let reason = match scenario {
                "missing-input" => BlockReason::Missing,
                "unauthorized-input" => BlockReason::Unauthorized,
                "unavailable-input" => BlockReason::Unavailable,
                "invalid-input" => BlockReason::Invalid,
                _ => return Err(Error::Invalid("unknown semantic case".into())),
            };
            every_input_blocks(plan, reaction, &case.input_ids, key, &reason)?
        }
    };
    Ok(passed)
}

fn every_input_blocks(
    plan: &CompiledPlan,
    reaction: &Reaction,
    input_ids: &[String],
    key: &RolloutKey,
    reason: &BlockReason,
) -> Result<bool> {
    for input_id in input_ids {
        let snapshot = match reason {
            BlockReason::Missing => InputSnapshot::Missing,
            BlockReason::Unauthorized => InputSnapshot::Unauthorized,
            BlockReason::Unavailable => InputSnapshot::Unavailable,
            BlockReason::Invalid => InputSnapshot::Invalid,
            BlockReason::ManagedSourceNotApplied | BlockReason::KeyUnavailable => {
                return Err(Error::Invalid(
                    "unsupported generated failure reason".into(),
                ));
            }
        };
        let mut values = snapshots(plan);
        values.insert(input_id.clone(), snapshot);
        let outcome =
            crate::rollout::evaluate_validated_rollout(plan, &reaction.target, &values, Some(key))?;
        if outcome
            != (RolloutOutcome::Blocked {
                input_id: input_id.clone(),
                reason: reason.clone(),
            })
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn every_input_changes(
    plan: &CompiledPlan,
    reaction: &Reaction,
    input_ids: &[String],
    key: &RolloutKey,
    baseline: &RolloutOutcome,
) -> Result<bool> {
    if ready(baseline).is_none() {
        return Ok(false);
    }
    for input_id in input_ids {
        let stimuli = BTreeMap::from([(input_id.clone(), "changed".to_owned())]);
        let outcome = stimulated_outcome(plan, reaction, key, &stimuli)?;
        if ready(&outcome).is_none() || &outcome == baseline {
            return Ok(false);
        }
    }
    Ok(true)
}

fn coalesced_inputs_use_latest_set(
    plan: &CompiledPlan,
    reaction: &Reaction,
    input_ids: &[String],
    key: &RolloutKey,
    baseline: &RolloutOutcome,
) -> Result<bool> {
    if ready(baseline).is_none() {
        return Ok(false);
    }
    let mut evolving = BTreeMap::new();
    let mut outcomes = Vec::new();
    for input_id in input_ids {
        evolving.insert(input_id.clone(), "intermediate".to_owned());
        outcomes.push(stimulated_outcome(plan, reaction, key, &evolving)?);
        evolving.insert(input_id.clone(), "latest".to_owned());
        outcomes.push(stimulated_outcome(plan, reaction, key, &evolving)?);
    }
    let Some(sequential_latest) = outcomes.last() else {
        return Ok(false);
    };
    let direct_latest = stimulated_outcome(plan, reaction, key, &evolving)?;
    Ok(ready(sequential_latest).is_some()
        && sequential_latest == &direct_latest
        && sequential_latest != baseline
        && outcomes
            .iter()
            .take(outcomes.len().saturating_sub(1))
            .any(|outcome| outcome != sequential_latest))
}

fn stimulated_outcome(
    plan: &CompiledPlan,
    reaction: &Reaction,
    key: &RolloutKey,
    changes: &BTreeMap<String, String>,
) -> Result<RolloutOutcome> {
    let mut changed_snapshots = snapshots(plan);
    for (input_id, marker) in changes {
        let input = plan
            .ir
            .inputs
            .iter()
            .find(|input| input.id == *input_id)
            .ok_or_else(|| Error::Invalid("reaction references an unknown input".into()))?;
        let value = if input.kind == "Secret" {
            base64::engine::general_purpose::STANDARD.encode(marker)
        } else {
            marker.clone()
        };
        changed_snapshots.insert(
            input_id.clone(),
            InputSnapshot::Available {
                data: json!({"fixture":value}),
                authoritative_applied: true,
            },
        );
    }
    crate::rollout::evaluate_validated_rollout(
        plan,
        &reaction.target,
        &changed_snapshots,
        Some(key),
    )
}
