//! The handler commit path end to end, on a real cluster: the uid-bound
//! commit, per-key Server-Side-Apply ownership of a handler output, and
//! seeding across a controller restart.
//!
//! Deploys `crates/acceptance/programs/handler-output`: one domain, one
//! observed `ConfigMap` input (`handler-source`, created and updated by
//! this test, never managed by the domain), one handler with no egress
//! that copies every key of the input's `data` into its declared output
//! (`handler-derived`). See docs/reference/ACCEPTANCE.md and
//! docs/reference/IR-CONTRACT.md, "Handlers and outputs".

use crate::support::writer_checkpoint;
use acceptance::{
    Error, Result, aps, ensure,
    objects::{Objects, pod_ready, text},
    pair::{CONTROL_NAMESPACE, Candidate, WORKLOAD_NAMESPACE, policy},
    scenario::{self, Scenario, note},
    wait,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

/// The domain `crates/acceptance/programs/handler-output/program.tsx`
/// declares.
const DOMAIN: &str = "handler-output";
/// The observed input this test creates and updates directly.
const SOURCE_NAME: &str = "handler-source";
/// The handler's one declared output.
const OUTPUT_NAME: &str = "handler-derived";
/// Mirrors `runtime::handlers::HANDLER_INPUT_REVISION_ANNOTATION`
/// (crate-private to `runtime`; this is the wire-level annotation key
/// IR-CONTRACT.md documents).
const REVISION_ANNOTATION: &str = "apsides.dev/handler-input-revision";
/// How long one handler dispatch (at most one per reconcile cycle) may
/// take to land.
const DISPATCH: Duration = Duration::from_secs(180);

/// Spec's handler runtime acceptance row (see docs/reference/ACCEPTANCE.md
/// and the Apsides v0.3 plan, Phase 5b runtime-integration review):
///
/// 1. The first invocation's written keys land exactly, with the revision
///    annotation and the expected field managers.
/// 2. Changing the input drops a key the handler no longer writes and
///    changes the revision.
/// 3. A key a different field manager owns survives the next handler
///    commit and is never co-owned by the handler's own manager.
/// 4. Restarting the controller Pod rewrites nothing: the output's
///    `resourceVersion` is unchanged, proving seeding from the stamped
///    annotation.
#[tokio::test]
#[ignore = "creates a kind cluster; needs Docker, kind and `cuenv task acceptancePrepare`"]
async fn a_handler_commits_exactly_its_owned_keys_and_seeds_across_a_restart() -> Result {
    scenario::run("handler-output", async |scenario| {
        let candidate = Candidate::read(&scenario.inputs.handler_output_release)?;
        exercise(scenario, &candidate).await
    })
    .await
}

async fn exercise(scenario: &mut Scenario<'_>, candidate: &Candidate) -> Result {
    let objects = scenario.objects.clone();
    let policies = BTreeMap::from([(DOMAIN.to_owned(), policy(&candidate.plan))]);
    let platform = scenario.prepare(&[candidate], &policies).await?;

    note("creating the observed input");
    objects
        .create(&json!({
            "apiVersion": "v1", "kind": "ConfigMap",
            "metadata": {"name": SOURCE_NAME, "namespace": WORKLOAD_NAMESPACE},
            "data": {"a": "1", "b": "2"},
        }))
        .await?;

    note("deploying the handler-output domain");
    let run = scenario
        .deploy(&platform, &candidate.root, "handler-output-initial")
        .await?;
    aps::committed(
        &run,
        &candidate.release_digest,
        &candidate.manifest.ir_digest,
    )?;

    let managers = Managers {
        handler: format!("apsides-handler-{DOMAIN}"),
        reconciler: format!("apsides-{DOMAIN}"),
    };
    let first = assert_first_invocation(scenario, &objects, &managers).await?;
    let second = assert_input_change_drops_a_key(scenario, &objects, &managers, &first).await?;
    assert_foreign_key_survives(scenario, &objects, &managers, &second).await?;
    assert_restart_seeds_and_rewrites_nothing(scenario, &objects, &platform).await
}

/// The handler's and the reconciler's own field managers for this domain.
struct Managers {
    handler: String,
    reconciler: String,
}

/// The revision annotation of `output`.
fn revision_of(output: &Value) -> &str {
    text(
        output,
        &format!("/metadata/annotations/{REVISION_ANNOTATION}"),
    )
}

/// (1) The first invocation's written keys land exactly, with the revision
/// annotation and the expected field managers.
async fn assert_first_invocation(
    scenario: &mut Scenario<'_>,
    objects: &Objects,
    managers: &Managers,
) -> Result<Value> {
    note("(1) the first invocation writes exactly {a,b}");
    let first = wait::until("the output to reflect {a,b}", DISPATCH, async || {
        let output = objects
            .require("v1", "ConfigMap", Some(WORKLOAD_NAMESPACE), OUTPUT_NAME)
            .await?;
        Ok((data_of(&output)
            == BTreeMap::from([("a".into(), "1".into()), ("b".into(), "2".into())])
            && !revision_of(&output).is_empty())
        .then_some(output))
    })
    .await?;
    ensure(
        owned_data_keys(&first, &managers.handler)
            == BTreeSet::from(["a".to_owned(), "b".to_owned()]),
        "the handler manager must own exactly a and b in managedFields",
    )?;
    ensure(
        manager_owns(&first, &managers.reconciler, &["f:metadata", "f:labels"]),
        "the reconciler's own manager must own the output's labels",
    )?;
    ensure(
        manager_owns(&first, &managers.handler, &["f:metadata", "f:annotations"]),
        "the handler manager must own the revision annotation",
    )?;
    scenario.recorder.check(
        "1-first-invocation-writes-exactly-its-keys",
        &json!({"data": first["data"], "revision": revision_of(&first)}),
    );
    Ok(first)
}

/// (2) Changing the input drops a key the handler no longer writes and
/// changes the revision.
async fn assert_input_change_drops_a_key(
    scenario: &mut Scenario<'_>,
    objects: &Objects,
    managers: &Managers,
    first: &Value,
) -> Result<Value> {
    note("(2) changing the input drops the key it no longer writes");
    let first_revision = revision_of(first).to_owned();
    update_source(objects, BTreeMap::from([("b", "2"), ("c", "3")])).await?;
    let second = wait::until("the output to reflect {b,c}", DISPATCH, async || {
        let output = objects
            .require("v1", "ConfigMap", Some(WORKLOAD_NAMESPACE), OUTPUT_NAME)
            .await?;
        Ok((data_of(&output)
            == BTreeMap::from([("b".into(), "2".into()), ("c".into(), "3".into())])
            && revision_of(&output) != first_revision
            && !revision_of(&output).is_empty())
        .then_some(output))
    })
    .await?;
    ensure(
        owned_data_keys(&second, &managers.handler)
            == BTreeSet::from(["b".to_owned(), "c".to_owned()]),
        "a is no longer among the handler manager's owned keys",
    )?;
    scenario.recorder.check(
        "2-changed-input-drops-the-key-it-no-longer-writes",
        &json!({"data": second["data"], "revision": revision_of(&second)}),
    );
    Ok(second)
}

/// (3) A key a different field manager owns survives the next handler
/// commit and is never co-owned by the handler's own manager.
async fn assert_foreign_key_survives(
    scenario: &mut Scenario<'_>,
    objects: &Objects,
    managers: &Managers,
    second: &Value,
) -> Result {
    note("(3) a foreign field manager's key survives the next commit");
    let second_revision = revision_of(second).to_owned();
    add_foreign_key(objects, second, "foreign", "kept-value").await?;
    update_source(objects, BTreeMap::from([("b", "2"), ("c", "4")])).await?;
    let third = wait::until(
        "the output to reflect {b,c} with c changed",
        DISPATCH,
        async || {
            let output = objects
                .require("v1", "ConfigMap", Some(WORKLOAD_NAMESPACE), OUTPUT_NAME)
                .await?;
            Ok((data_of(&output)
                == BTreeMap::from([
                    ("b".into(), "2".into()),
                    ("c".into(), "4".into()),
                    ("foreign".into(), "kept-value".into()),
                ])
                && revision_of(&output) != second_revision
                && !revision_of(&output).is_empty())
            .then_some(output))
        },
    )
    .await?;
    ensure(
        owned_data_keys(&third, &managers.handler)
            == BTreeSet::from(["b".to_owned(), "c".to_owned()]),
        "the handler manager must never become a co-owner of the foreign key",
    )?;
    scenario.recorder.check(
        "3-foreign-key-survives-and-is-never-co-owned",
        &json!({"data": third["data"], "handlerOwnedKeys": ["b", "c"]}),
    );
    Ok(())
}

/// (4) Restarting the controller Pod rewrites nothing: the output's
/// `resourceVersion` is unchanged, proving seeding from the stamped
/// annotation.
async fn assert_restart_seeds_and_rewrites_nothing(
    scenario: &mut Scenario<'_>,
    objects: &Objects,
    platform: &acceptance::bootstrap::Platform,
) -> Result {
    note("(4) restarting the controller Pod rewrites nothing");
    let before_restart = objects
        .require("v1", "ConfigMap", Some(WORKLOAD_NAMESPACE), OUTPUT_NAME)
        .await?;
    let before_version = text(&before_restart, "/metadata/resourceVersion").to_owned();
    let checkpoint = writer_checkpoint(&platform.registry().await?)?.clone();
    let account = checkpoint.runtime_binding.service_account_name.clone();
    let controller_pod = objects
        .list("v1", "Pod", Some(CONTROL_NAMESPACE), None)
        .await?
        .into_iter()
        .find(|pod| text(pod, "/spec/serviceAccountName") == account)
        .ok_or_else(|| Error::Check("the controller Pod is absent".into()))?;
    let old_uid = text(&controller_pod, "/metadata/uid").to_owned();
    objects.delete(&controller_pod).await?;
    wait::until(
        "a replacement controller Pod to become ready",
        DISPATCH,
        async || {
            let pods = objects
                .list("v1", "Pod", Some(CONTROL_NAMESPACE), None)
                .await?;
            Ok(pods.into_iter().find(|pod| {
                text(pod, "/spec/serviceAccountName") == account
                    && text(pod, "/metadata/uid") != old_uid
                    && pod_ready(pod)
            }))
        },
    )
    .await?;
    // No new revision to react to and the previous invocation's revision
    // is already seeded from the stamped annotation -- give the restarted
    // controller several reconcile cycles to prove it stays idle, then
    // require the output's resourceVersion to be exactly what it was
    // before the restart.
    tokio::time::sleep(Duration::from_secs(20)).await;
    let after_restart = objects
        .require("v1", "ConfigMap", Some(WORKLOAD_NAMESPACE), OUTPUT_NAME)
        .await?;
    ensure(
        text(&after_restart, "/metadata/resourceVersion") == before_version,
        "the output must not be rewritten across a controller restart (seeding)",
    )?;
    scenario.recorder.check(
        "4-restart-seeds-and-rewrites-nothing",
        &json!({"resourceVersion": before_version}),
    );
    Ok(())
}

/// Replace the observed input's `data` with exactly `data`.
async fn update_source(objects: &Objects, data: BTreeMap<&str, &str>) -> Result {
    let mut current = objects
        .require("v1", "ConfigMap", Some(WORKLOAD_NAMESPACE), SOURCE_NAME)
        .await?;
    current["data"] = json!(data);
    objects.replace(&current).await?;
    Ok(())
}

/// Add `key`/`value` to `output`'s live `data` with a plain (non-apply)
/// replace, as `kubectl edit`/`kubectl apply` (never server-side apply)
/// would -- the field manager this lands under is whatever the client's
/// own default is, distinct from both the reconciler's and the handler's
/// own named managers either way.
async fn add_foreign_key(objects: &Objects, output: &Value, key: &str, value: &str) -> Result {
    let mut current = objects
        .require("v1", "ConfigMap", Some(WORKLOAD_NAMESPACE), OUTPUT_NAME)
        .await?;
    ensure(
        current["metadata"]["resourceVersion"] == output["metadata"]["resourceVersion"],
        "the output changed before the foreign key could be added",
    )?;
    current["data"][key] = json!(value);
    objects.replace(&current).await?;
    Ok(())
}

/// `object`'s `data` map as owned strings, for exact comparison.
fn data_of(object: &Value) -> BTreeMap<String, String> {
    object["data"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_owned())))
        .collect()
}

/// The subset of `object`'s live `data` keys that `manager` owns, read
/// from its own `managedFields` `Apply` entry -- mirrors
/// `runtime::production::handler_owned_data_keys`.
fn owned_data_keys(object: &Value, manager: &str) -> BTreeSet<String> {
    object["metadata"]["managedFields"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["manager"] == manager && entry["operation"] == "Apply")
        .filter_map(|entry| entry["fieldsV1"].get("f:data"))
        .filter_map(Value::as_object)
        .flat_map(|fields| fields.keys().cloned())
        .filter_map(|key| key.strip_prefix("f:").map(str::to_owned))
        .collect()
}

/// Whether `manager` has an `Apply` entry whose `fieldsV1` contains the
/// nested path `path` (each segment already `f:`-prefixed).
fn manager_owns(object: &Value, manager: &str, path: &[&str]) -> bool {
    object["metadata"]["managedFields"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["manager"] == manager && entry["operation"] == "Apply")
        .any(|entry| {
            let mut cursor = &entry["fieldsV1"];
            for segment in path {
                cursor = &cursor[*segment];
            }
            !cursor.is_null()
        })
}
