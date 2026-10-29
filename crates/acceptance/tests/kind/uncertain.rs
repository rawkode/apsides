//! Uncertain writer termination.

use crate::{
    support::{stop_at, stored_phase, writer_checkpoint},
    upgrade::create_api_settings,
};
use acceptance::{
    Error, Result, aps,
    bootstrap::Platform,
    cnpg, ensure,
    objects::{Objects, text},
    observe::{control_objects, metadata_rows},
    pair::{CONTROL_NAMESPACE, DOMAIN, Pair, WORKLOAD_NAMESPACE},
    scenario::{self, Scenario, note},
    wait,
};
use deploy::Phase;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, time::Duration};

/// The finalizer the platform keeps on a quiesced writer's Pods so their
/// termination can be verified.
const EVIDENCE_FINALIZER: &str = "apsides.dev/termination-evidence";
/// How activation refuses when a protected writer Pod record is gone.
const REFUSAL: &str = "live writer set differs from complete protected Pod history";

/// Spec §28 "Lose writer termination evidence before activation": `aps
/// deploy` of the successor is stopped right after its registry write into
/// Quiesced, before activation transfers any claim. The predecessor
/// controller's retained terminal Pod record, the platform's termination
/// evidence, is then removed. Rerunning `aps deploy` must refuse activation
/// for the missing protected writer history, leave the successor runtime at
/// zero replicas with no Pod, and leave the registry, activation and
/// inventory `ConfigMap`s and every metadata database row unchanged.
#[tokio::test]
#[ignore = "creates a kind cluster; needs kind on podman or Docker and `cuenv task acceptancePrepare`"]
async fn lost_writer_evidence_blocks_activation() -> Result {
    scenario::run("uncertain-writer", async |scenario| {
        let pair = Pair::load(
            &scenario.inputs.initial_release,
            &scenario.inputs.successor_release,
        )?;
        uncertain_writer(scenario, &pair).await
    })
    .await
}

async fn uncertain_writer(scenario: &mut Scenario<'_>, pair: &Pair) -> Result {
    let policies = BTreeMap::from([(DOMAIN.to_owned(), pair.successor_policy.clone())]);
    let platform = scenario
        .prepare(&[&pair.initial, &pair.successor], &policies)
        .await?;
    let objects = scenario.objects.clone();
    let run = scenario
        .deploy(&platform, &pair.initial.root, "deploy-initial")
        .await?;
    let initial = aps::committed(
        &run,
        &pair.initial.release_digest,
        &pair.initial.manifest.ir_digest,
    )?;
    cnpg::wait_ready(&objects, WORKLOAD_NAMESPACE).await?;
    create_api_settings(&objects).await?;
    let predecessor = writer_checkpoint(&platform.registry().await?, DOMAIN)?.clone();

    note("stopping the successor at Quiesced");
    let stopped = stop_at(
        scenario,
        &platform,
        &pair.successor,
        Phase::Quiesced,
        "quiesce",
    )
    .await?;
    let registry = platform.registry().await?;
    let stored = registry.value.transition(&stopped.transition)?;
    let writers = &stored
        .upgrade_drain
        .as_ref()
        .ok_or_else(|| Error::Check("the quiesced transition has no drain record".into()))?
        .runtime_writers;
    let [pod] = writers.pods.as_slice() else {
        return Err(Error::Check(
            "the protected writer history is not one Pod".into(),
        ));
    };
    ensure(
        writers.deployment_uid == predecessor.controller_deployment_uid
            && writers.service_account_name
                == format!(
                    "{}-process",
                    predecessor.runtime_binding.service_account_name
                ),
        "the protected writer history is not the initial controller",
    )?;
    let successor = stopped_successor(&objects, &stopped.transition, &platform).await?;
    let names = [
        format!("apsides-registry-{WORKLOAD_NAMESPACE}"),
        format!("apsides-activation-{DOMAIN}"),
        format!("apsides-inventory-{DOMAIN}"),
    ];
    let before = protected_state(&objects, &names, &scenario.database, &initial.generation).await?;

    note("removing the predecessor's retained Pod record");
    let fault = lose_pod_record(&objects, &pod.name, &pod.uid).await?;
    let injected =
        protected_state(&objects, &names, &scenario.database, &initial.generation).await?;
    ensure(
        injected == before,
        "removing the Pod record changed protected state",
    )?;

    note("resuming the successor");
    let run = scenario
        .deploy(&platform, &pair.successor.root, "resume-refused")
        .await?;
    aps::refused(&run, REFUSAL)?;
    let after = protected_state(&objects, &names, &scenario.database, &initial.generation).await?;
    ensure(
        after == before,
        "the refused activation changed protected state",
    )?;
    ensure(
        stopped_successor(&objects, &stopped.transition, &platform).await? == successor,
        "the refused activation started or changed the successor",
    )?;
    ensure(
        stored_phase(&objects, &stopped.transition)
            .await?
            .1
            .as_deref()
            == Some("Quiesced")
            && objects
                .get("v1", "Pod", Some(CONTROL_NAMESPACE), &pod.name)
                .await?
                .is_none(),
        "the refused activation moved the transition or restored the Pod record",
    )?;
    scenario.recorder.check(
        "lost-writer-evidence-blocks-activation",
        &json!({
            "stop": stopped.evidence,
            "fault": fault,
            "refusal": REFUSAL,
            "successor": successor,
            "protectedState": before,
        }),
    );
    Ok(())
}

/// The registry, activation and inventory `ConfigMap`s and the metadata
/// database rows, after checking activation still names the draining
/// predecessor.
async fn protected_state(
    objects: &Objects,
    names: &[String],
    database: &Path,
    predecessor: &str,
) -> Result<Value> {
    let activation = objects
        .require("v1", "ConfigMap", Some(CONTROL_NAMESPACE), &names[1])
        .await?;
    let state: Value = serde_json::from_str(text(&activation, "/data/state.json"))?;
    ensure(
        state["status"] == "draining" && state["binding"]["generationId"] == predecessor,
        "activation is not the draining predecessor",
    )?;
    Ok(json!({
        "objects": control_objects(objects, names).await?,
        "metadata": metadata_rows(database)?,
    }))
}

/// The staged successor runtime at zero replicas with no Pod of its process
/// account, and its proxy.
async fn stopped_successor(
    objects: &Objects,
    transition: &str,
    platform: &Platform,
) -> Result<Value> {
    let registry = platform.registry().await?;
    let stored = registry.value.transition(transition)?;
    let account = stored.allocation().service_account_name.clone();
    let deployment = objects
        .require("apps/v1", "Deployment", Some(CONTROL_NAMESPACE), &account)
        .await?;
    let process = text(&deployment, "/spec/template/spec/serviceAccountName").to_owned();
    ensure(
        deployment["spec"]["replicas"] == 0
            && deployment["metadata"]["deletionTimestamp"].is_null()
            && process == format!("{account}-process"),
        "the successor runtime is not the stopped staged identity",
    )?;
    let started = objects
        .list("v1", "Pod", Some(CONTROL_NAMESPACE), None)
        .await?
        .iter()
        .any(|pod| text(pod, "/spec/serviceAccountName") == process);
    ensure(!started, "a successor runtime Pod was started")?;
    let proxy = stored
        .proxy_identity
        .as_ref()
        .ok_or_else(|| Error::Check("the successor has no staged proxy".into()))?;
    let proxy_deployment = objects
        .require(
            "apps/v1",
            "Deployment",
            Some(CONTROL_NAMESPACE),
            &proxy.deployment_name,
        )
        .await?;
    ensure(
        text(&proxy_deployment, "/metadata/uid") == proxy.deployment_uid,
        "the successor proxy differs from its protected identity",
    )?;
    Ok(json!({
        "runtime": account,
        "runtimeUid": deployment["metadata"]["uid"],
        "replicas": 0,
        "proxyDeploymentUid": proxy.deployment_uid,
    }))
}

/// Delete the terminal predecessor Pod and remove the platform's evidence
/// finalizer from it, as an administrator losing that record would.
async fn lose_pod_record(objects: &Objects, name: &str, uid: &str) -> Result<Value> {
    let pod = objects
        .require("v1", "Pod", Some(CONTROL_NAMESPACE), name)
        .await?;
    let terminal = matches!(text(&pod, "/status/phase"), "Succeeded" | "Failed");
    let finalized = pod["metadata"]["finalizers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|finalizer| *finalizer == EVIDENCE_FINALIZER)
        .count()
        == 1;
    ensure(
        text(&pod, "/metadata/uid") == uid && terminal && finalized,
        "the fault needs the exact retained terminal predecessor Pod",
    )?;
    if pod["metadata"]["deletionTimestamp"].is_null() {
        objects.delete(&pod).await?;
    }
    let mut pod = objects
        .require("v1", "Pod", Some(CONTROL_NAMESPACE), name)
        .await?;
    ensure(text(&pod, "/metadata/uid") == uid, "the Pod was replaced")?;
    let kept = pod["metadata"]["finalizers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|finalizer| *finalizer != EVIDENCE_FINALIZER)
        .cloned()
        .collect::<Vec<_>>();
    pod["metadata"]["finalizers"] = json!(kept);
    objects.replace(&pod).await?;
    wait::until(
        "the predecessor Pod record to disappear",
        Duration::from_secs(60),
        async || {
            Ok(objects
                .get("v1", "Pod", Some(CONTROL_NAMESPACE), name)
                .await?
                .is_none()
                .then_some(()))
        },
    )
    .await?;
    Ok(json!({"pod": name, "uid": uid, "removedFinalizer": EVIDENCE_FINALIZER, "absent": true}))
}
