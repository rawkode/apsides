use super::*;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

pub(super) fn admission_reads(registry: &mut LifecycleRegistry) -> TestResult<Vec<Value>> {
    let objects = qualified_writer::qualify(registry, TRANSITION_A)?;
    let checkpoint = fixture_writer_checkpoint(registry)?;
    registry
        .active_writer_checkpoints
        .insert("example".into(), checkpoint.clone());
    if let Some(stored) = registry.transitions.get_mut(TRANSITION_B) {
        stored.previous_writer_checkpoint = Some(checkpoint.clone());
        registry.admit(TRANSITION_B)?;
    }
    let find = |kind: &str, name: &str| -> TestResult<Value> {
        objects
            .iter()
            .find(|value| value["kind"] == kind && value["metadata"]["name"] == name)
            .cloned()
            .ok_or_else(|| format!("missing {kind}/{name} fixture").into())
    };
    let proxy = &checkpoint.proxy_identity;
    let node = find("Node", &proxy.node_name)?;
    let policy = policy(&registry.plan(TRANSITION_A)?.request.next.compiled);
    let mut active = activation(&registry.plan(TRANSITION_A)?.request.next.compiled, &policy);
    active.binding = checkpoint.runtime_binding.clone();
    active.status = ActivationStatus::Active;
    let mut reads = vec![
        object(
            "reakt-activation-example",
            &active,
            &checkpoint.activation_uid,
            "45",
        ),
        find("ConfigMap", "reakt-proxy-policy-example")?,
        node.clone(),
        find("Pod", &proxy.pod_name)?,
        find("ConfigMap", &proxy.config_map_name)?,
    ];
    let name = &checkpoint.runtime_binding.service_account_name;
    let account = format!("{name}-process");
    let deployment = json!({"apiVersion":"apps/v1","kind":"Deployment",
        "metadata":{"name":name,"namespace":"control","uid":checkpoint.controller_deployment_uid,"resourceVersion":"51"},
        "spec":{"replicas":1,"selector":{"matchLabels":{"app":"runtime"}},"template":{"metadata":{"labels":{"app":"runtime"}},"spec":{"serviceAccountName":account,"containers":[{"name":"controller","image":"runtime"}]}}}});
    let pod = json!({"apiVersion":"v1","kind":"Pod",
        "metadata":{"name":"runtime-pod","namespace":"control","uid":"runtime-pod-uid","resourceVersion":"52",
            "finalizers":[TERMINATION_FINALIZER],"ownerReferences":[{"apiVersion":"apps/v1","kind":"ReplicaSet","name":"runtime-rs","uid":"runtime-rs-uid","controller":true}]},
        "spec":{"serviceAccountName":account,"nodeName":proxy.node_name,"containers":[{"name":"controller","image":"runtime"}]},
        "status":{"phase":"Running","containerStatuses":[{"name":"controller","image":"runtime","imageID":"runtime","containerID":"containerd://runtime","ready":true,"restartCount":0,"state":{"running":{}}}]}});
    let replica = json!({"apiVersion":"apps/v1","kind":"ReplicaSet",
        "metadata":{"name":"runtime-rs","namespace":"control","uid":"runtime-rs-uid","resourceVersion":"53",
            "ownerReferences":[{"apiVersion":"apps/v1","kind":"Deployment","name":name,"uid":checkpoint.controller_deployment_uid,"controller":true}]}});
    for _ in 0..2 {
        reads.extend([
            deployment.clone(),
            json!({"apiVersion":"v1","kind":"PodList","metadata":{},"items":[pod.clone()]}),
            replica.clone(),
            node.clone(),
        ]);
    }
    Ok(reads)
}

#[tokio::test]
async fn profile_selection_accounts_for_retirement_uid_width() -> TestResult {
    let mut registry = qualified_upgrade_fixture()?;
    let responses = admission_reads(&mut registry)?;
    let mut checkpoint = registry
        .active_writer_checkpoints
        .get("example")
        .ok_or("checkpoint missing")?
        .clone();
    let uid = checkpoint
        .retirement_identity_uids
        .values_mut()
        .next()
        .ok_or("retirement identity missing")?;
    *uid = "u".repeat(130);
    let (client, calls) = wire(responses);
    let profile = platform_client(client, "control", "workload", "cluster-a")?
        .select_evidence_profile(Some(&checkpoint))
        .await?;
    assert_eq!(profile.max_uid_json_bytes, 512);
    assert!(
        calls
            .lock()
            .map_err(|_| "calls lock poisoned")?
            .iter()
            .all(|call| call.0 == "GET")
    );
    Ok(())
}

#[tokio::test]
async fn topology_capture_rejects_owner_substitution_and_container_epoch_drift() -> TestResult {
    for drift in [false, true] {
        let mut registry = qualified_upgrade_fixture()?;
        let mut responses = admission_reads(&mut registry)?;
        if drift {
            let pod_list = responses
                .iter_mut()
                .rev()
                .find(|value| value["kind"] == "PodList")
                .ok_or("pod list missing")?;
            pod_list["items"][0]["status"]["containerStatuses"][0]["containerID"] =
                json!("containerd://restarted");
        } else {
            let replica = responses
                .iter_mut()
                .find(|value| value["kind"] == "ReplicaSet")
                .ok_or("replica missing")?;
            replica["metadata"]["ownerReferences"][0]["uid"] = json!("replacement-deployment");
        }
        let checkpoint = registry
            .active_writer_checkpoints
            .get("example")
            .ok_or("checkpoint missing")?;
        let (client, calls) = wire(responses);
        let error = platform_client(client, "control", "workload", "cluster-a")?
            .select_evidence_profile(Some(checkpoint))
            .await
            .err()
            .ok_or("changed topology accepted")?;
        assert!(
            error.to_string().contains(if drift {
                "topology changed"
            } else {
                "ownership"
            }),
            "{error}"
        );
        assert!(
            calls
                .lock()
                .map_err(|_| "calls lock poisoned")?
                .iter()
                .all(|call| call.0 == "GET")
        );
    }
    Ok(())
}

#[tokio::test]
async fn resize_changes_only_capacity_and_invalidates_the_old_session() -> TestResult {
    let (registry, responses) = resize_responses("grow")?;
    let (client, calls) = wire(responses);
    let platform = registry.bind(client, "control", "workload", "cluster-a", TRANSITION_B)?;
    let written =
        Box::pin(platform.resize_pre_quiescence_capacity(TRANSITION_B, "executor-b")).await?;
    let capacity = &written.value.transition(TRANSITION_B)?.capacity;
    let original = &registry.transition(TRANSITION_B)?.capacity;
    assert_eq!(capacity.revision, original.revision + 1);
    assert_eq!(capacity.profile.max_uid_json_bytes, 512);
    assert!(capacity.profile.contains(&original.profile));
    let mut expected = registry.registry.clone();
    expected
        .transitions
        .get_mut(TRANSITION_B)
        .ok_or("pending missing")?
        .capacity = capacity.clone();
    assert_eq!(written.value, expected);
    assert!(platform.selected(&written, TRANSITION_B).is_err());
    let calls = calls.lock().map_err(|_| "calls lock poisoned")?;
    let writes = calls
        .iter()
        .filter(|call| call.0 != "GET")
        .collect::<Vec<_>>();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].0, "PUT");
    assert_eq!(
        writes[0].1.parse::<http::Uri>()?.path(),
        "/api/v1/namespaces/control/configmaps/reakt-registry-workload"
    );
    Ok(())
}

#[tokio::test]
async fn resize_rechecks_active_predecessor_and_respects_registry_cas() -> TestResult {
    for mode in ["draining-before", "draining-after", "registry-race"] {
        let (registry, responses) = resize_responses(mode)?;
        let (client, calls) = wire(responses);
        let platform = registry.bind(client, "control", "workload", "cluster-a", TRANSITION_B)?;
        let error = Box::pin(platform.resize_pre_quiescence_capacity(TRANSITION_B, "executor-b"))
            .await
            .err()
            .ok_or("unsafe resize accepted")?;
        assert!(
            error.to_string().contains(if mode == "registry-race" {
                "changed"
            } else {
                "Active activation"
            }),
            "{mode}: {error}"
        );
        assert!(
            calls
                .lock()
                .map_err(|_| "calls lock poisoned")?
                .iter()
                .all(|call| call.0 == "GET")
        );
    }
    Ok(())
}

#[tokio::test]
async fn unchanged_resize_is_read_only_and_keeps_the_session() -> TestResult {
    let (registry, responses) = resize_responses("unchanged")?;
    let (client, calls) = wire(responses);
    let platform = registry.bind(client, "control", "workload", "cluster-a", TRANSITION_B)?;
    let written =
        Box::pin(platform.resize_pre_quiescence_capacity(TRANSITION_B, "executor-b")).await?;
    assert_eq!(written.value, registry.registry);
    platform.selected(&written, TRANSITION_B)?;
    assert!(
        calls
            .lock()
            .map_err(|_| "calls lock poisoned")?
            .iter()
            .all(|call| call.0 == "GET")
    );
    Ok(())
}

fn resize_responses(mode: &str) -> TestResult<(LifecycleRegistry, Vec<Value>)> {
    let mut registry = qualified_upgrade_fixture()?;
    let mut topology = admission_reads(&mut registry)?;
    if mode != "unchanged" {
        for value in topology
            .iter_mut()
            .filter(|value| value["kind"] == "PodList")
        {
            value["items"][0]["metadata"]["uid"] = json!("u".repeat(130));
        }
    }
    let previous = registry
        .active_writer_checkpoints
        .get("example")
        .ok_or("checkpoint missing")?;
    let policy = policy(&registry.plan(TRANSITION_B)?.request.next.compiled);
    let mut activation = activation(&registry.plan(TRANSITION_A)?.request.next.compiled, &policy);
    activation.binding = previous.runtime_binding.clone();
    activation.status = ActivationStatus::Active;
    let active = object(
        "reakt-activation-example",
        &activation,
        &previous.activation_uid,
        "45",
    );
    activation.status = ActivationStatus::Draining;
    let draining = object(
        "reakt-activation-example",
        &activation,
        &previous.activation_uid,
        "46",
    );
    let ceiling = object("reakt-policy-example", &policy, "policy-uid", "44");
    let protected = registry_object(&registry, REGISTRY_UID, "42")?;
    let mut responses = vec![
        protected.clone(),
        ceiling.clone(),
        if mode == "draining-before" {
            draining.clone()
        } else {
            active.clone()
        },
    ];
    responses.extend(topology);
    responses.extend([
        if mode == "draining-after" {
            draining
        } else {
            active
        },
        ceiling,
    ]);
    responses.push(if mode == "registry-race" {
        registry_object(&registry, REGISTRY_UID, "43")?
    } else {
        protected
    });
    responses.push(json!("echo-put"));
    Ok((registry, responses))
}

#[tokio::test]
async fn capacity_resize_rejects_a_different_executor_before_live_capture() -> TestResult {
    let (registry, responses) = resize_responses("grow")?;
    let (client, calls) = wire(responses);
    let platform = registry.bind(client, "control", "workload", "cluster-a", TRANSITION_B)?;
    let error =
        Box::pin(platform.resize_pre_quiescence_capacity(TRANSITION_B, "different-executor"))
            .await
            .err()
            .ok_or("foreign executor resized capacity")?;
    assert!(error.to_string().contains("owning untouched"), "{error}");
    let calls = calls.lock().map_err(|_| "calls lock poisoned")?;
    assert!(calls.iter().all(|call| call.0 == "GET"));
    assert!(
        !calls
            .iter()
            .any(|call| call.1.contains("/pods") || call.1.contains("/deployments"))
    );
    Ok(())
}

#[tokio::test]
async fn image_pull_secret_topology_mutation_is_rejected() -> TestResult {
    let mut registry = qualified_upgrade_fixture()?;
    let mut responses = admission_reads(&mut registry)?;
    let pods = responses
        .iter_mut()
        .find(|v| v["kind"] == "PodList")
        .ok_or("Pod list missing")?;
    pods["items"][0]["spec"]["imagePullSecrets"] = json!([{"name":"unapproved"}]);
    let checkpoint = registry
        .active_writer_checkpoints
        .get("example")
        .ok_or("checkpoint missing")?;
    let (client, _) = wire(responses);
    let error = platform_client(client, "control", "workload", "cluster-a")?
        .select_evidence_profile(Some(checkpoint))
        .await
        .err()
        .ok_or("mutated pull references accepted")?;
    assert!(error.to_string().contains("image pull Secret references"));
    Ok(())
}
