use crate::{InputMode, Ir, Reaction, TestCase};

type Scenario = (&'static str, &'static str, &'static str);
const COMMON: &[Scenario] = &[
    (
        "initial-evaluation",
        "semantic",
        "available projected inputs produce a stable revision",
    ),
    (
        "irrelevant-change",
        "semantic",
        "metadata outside the projection cannot affect revision",
    ),
    (
        "same-value-replay",
        "semantic",
        "repeated equal input produces exactly equal revision",
    ),
    (
        "accepted-write-restart",
        "workload",
        "accepted template revision is unchanged after restart",
    ),
    (
        "field-conflict",
        "api",
        "competing field manager blocks without forced takeover",
    ),
    (
        "out-of-scope-change",
        "api",
        "runtime credentials cannot mutate undeclared targets",
    ),
    (
        "rollout-readiness",
        "workload",
        "real new-template Pods reach fixture readiness",
    ),
];
/// Scenarios that apply identically to `Observed` and `Output`-mode inputs:
/// both are observed live (see `model::InputMode::Output`), so both a
/// foreign writer's change and a handler's own write change the revision the
/// same way, and a controller restart relists and converges on either one's
/// latest live value the same way. For a reaction whose inputs span both
/// modes, one row is generated per scenario here, covering every relevant
/// input (`Observed` and `Output` together) so `(reactionId, scenario)`
/// stays unique -- see `OBSERVED_ONLY` below for scenarios that cannot be
/// merged this way.
const LIVE_SHARED: &[Scenario] = &[
    (
        "projected-change",
        "semantic",
        "changed projected data changes the revision",
    ),
    (
        "restart-and-relist",
        "workload",
        "controller restart converges from changes made while stopped",
    ),
];
/// Scenarios that apply only to `Observed`-mode inputs, never to `Output`
/// ones, because each models a failure mode specific to an *external*
/// writer's input:
///
/// - `missing-input`/`input-returned` (the missing/recovery pair): a handler
///   output is a managed resource the reconciler itself recreates
///   absent-only every cycle, so its metadata is never durably missing the
///   way an external `ConfigMap` can be; there is nothing for these to exercise.
/// - `unauthorized-input`/`credential-read-denial`: the reconciler's own
///   in-domain credentials always hold the get/list/watch capability an
///   output's Output-mode input adds (see `validate_output_wiring`), unlike
///   an external input whose access an operator could plausibly deny.
/// - `unavailable-input`/`invalid-input`: modeled failure injection points
///   for an external transport/projection this increment has no handler
///   runtime to trigger for an output specifically.
/// - `coalesced-inputs`: exercises batching several *independent* external
///   writers' changes together; an output has exactly one writer (its
///   handler, `validate_handlers`'s one-writer-per-output rule), so there is
///   only ever one change to coalesce.
///
/// Applicability here is always the reaction's `Observed`-mode inputs alone,
/// even for a mixed reaction, since none of these can involve an `Output`
/// one.
const OBSERVED_ONLY: &[Scenario] = &[
    (
        "coalesced-inputs",
        "semantic",
        "evaluation uses the latest complete input set",
    ),
    (
        "missing-input",
        "semantic",
        "missing required input blocks dependent mutation",
    ),
    (
        "input-returned",
        "semantic",
        "returning required input recovers the stable revision",
    ),
    (
        "unauthorized-input",
        "semantic",
        "typed unauthorized observation blocks dependent mutation; this is not API authorization evidence",
    ),
    (
        "credential-read-denial",
        "api",
        "actual runtime credentials cannot read a declared input denied by policy and dependent mutation remains blocked",
    ),
    (
        "unavailable-input",
        "semantic",
        "transport failure is distinguishable from absence",
    ),
    (
        "invalid-input",
        "semantic",
        "invalid projection blocks without exposing contents",
    ),
];
const MANAGED: &[Scenario] = &[
    (
        "managed-drift-repair",
        "workload",
        "external managed-source data drift is repaired to declared intent without changing revision",
    ),
    (
        "managed-missing",
        "api",
        "missing managed source with denied repair blocks dependent mutation without changing revision",
    ),
    (
        "managed-read-denial",
        "api",
        "actual runtime credentials denied managed-source reads block dependent mutation without changing revision",
    ),
    (
        "managed-unavailable",
        "api",
        "unavailable managed-source reads block dependent mutation without changing revision",
    ),
];

pub(crate) fn generate(ir: &Ir) -> Vec<TestCase> {
    let mut cases = Vec::new();
    for reaction in &ir.reactions {
        let mut all = reaction.inputs.clone();
        all.sort();
        let mode_ids = |mode: InputMode| {
            all.iter()
                .filter(|id| {
                    ir.inputs
                        .iter()
                        .any(|input| &input.id == *id && input.mode == mode)
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        let force = ir
            .resources
            .iter()
            .any(|r| r.id == reaction.target && r.force);
        for scenario in COMMON {
            let selected = if force && scenario.0 == "field-conflict" {
                &(
                    "forced-field-conflict",
                    "api",
                    "approved force reconciles competing fields while preserving object UID",
                )
            } else {
                scenario
            };
            append(&mut cases, reaction, std::slice::from_ref(selected), &all);
        }
        let observed = mode_ids(InputMode::Observed);
        let output = mode_ids(InputMode::Output);
        // Observed and Output ids never overlap (disjoint modes), so a plain
        // concatenation is already the union; re-sorted so the row's
        // `input_ids` stay in canonical order regardless of which mode
        // contributed them.
        let mut live = observed.clone();
        live.extend(output);
        live.sort();
        append(&mut cases, reaction, LIVE_SHARED, &live);
        append(&mut cases, reaction, OBSERVED_ONLY, &observed);
        let managed = mode_ids(InputMode::Managed);
        for scenario in MANAGED {
            let applicable = if scenario.0 == "managed-drift-repair" {
                managed
                    .iter()
                    .filter(|id| has_declared_data(ir, id))
                    .cloned()
                    .collect()
            } else {
                managed.clone()
            };
            append(
                &mut cases,
                reaction,
                std::slice::from_ref(scenario),
                &applicable,
            );
        }
    }
    cases.sort_by(|a, b| (&a.reaction_id, &a.scenario).cmp(&(&b.reaction_id, &b.scenario)));
    cases
}

fn append(
    cases: &mut Vec<TestCase>,
    reaction: &Reaction,
    scenarios: &[Scenario],
    inputs: &[String],
) {
    if inputs.is_empty() {
        return;
    }
    cases.extend(
        scenarios
            .iter()
            .map(|(scenario, layer, assertion)| TestCase {
                reaction_id: reaction.id.clone(),
                input_ids: inputs.to_vec(),
                source: reaction.source.clone(),
                scenario: (*scenario).into(),
                layer: (*layer).into(),
                assertion: (*assertion).into(),
                required: true,
            }),
    );
}

fn has_declared_data(ir: &Ir, input_id: &str) -> bool {
    let resource_id = ir
        .inputs
        .iter()
        .find(|input| input.id == input_id)
        .and_then(|input| input.managed_resource_id.as_ref());
    ir.resources
        .iter()
        .find(|resource| Some(&resource.id) == resource_id)
        .and_then(|resource| resource.desired.get("data"))
        .and_then(serde_json::Value::as_object)
        .is_some_and(|data| !data.is_empty())
}
