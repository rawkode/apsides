//! The generation's runtime credential at the API server, and the platform's
//! image pull secrets on the writers it stages.

use crate::support::{committed_report, nothing_leaks, writer_checkpoint};
use acceptance::{
    Error, Result, aps, cnpg,
    credentials::{self, Authenticated, Binding},
    ensure,
    objects::{Objects, text},
    pair::{CONTROL_NAMESPACE, DOMAIN, Pair, WORKLOAD_NAMESPACE},
    scenario::{self, Scenario, note},
    workload::Workload,
};
use platform::WriterCheckpoint;
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};

/// The registry credential the platform policy names for image pulls. The
/// images are already on the node, so the kubelet never uses it.
const PULL_SECRET: &str = "acceptance-registry";
/// The least time a token read from the proxy may have left.
const TOKEN_MARGIN_SECONDS: i64 = 300;

/// Spec §28 "Exercise runtime credentials" and "Pull a controller image
/// with platform credentials". Under a policy naming an image pull secret,
/// `aps deploy` of the CNPG successor commits, and:
///
/// (a) the controller and proxy Deployments it staged, and the proxy Pod,
///     carry exactly the policy's `imagePullSecrets`;
/// (b) the token the kubelet projected into the running proxy's envoy
///     container, read through the API server and never written anywhere,
///     is authenticated by a `TokenReview` as the generation's runtime
///     `ServiceAccount`, bound to exactly that proxy Pod;
/// (c) with that token, every capability the compiled plan declares
///     succeeds, each RBAC gap the compiler discloses behaves as disclosed
///     (a create under another name, a patch and an update of a field the
///     program never sets), a dry-run apply of every desired object
///     succeeds, and requests outside the declared scope are denied; every
///     write is a dry run;
/// (d) a `SubjectAccessReview` of each of those requests for the same user
///     and groups gives the same answer;
/// (e) the disclosed `sensitive-revision-change-observable` gap holds: adding
///     a key to the observed Secret rolls the API out to a new revision, and
///     removing it restores the original revision;
/// (f) the proxy Pod and `ServiceAccount` are unchanged throughout;
/// (g) the projected token, the pull secret and every other Secret value or
///     rollout key appear nowhere in `aps` output, the controllers' logs or
///     the inventory database.
#[tokio::test]
#[ignore = "creates a kind cluster; needs Docker, kind and `cuenv task acceptancePrepare`"]
async fn the_runtime_credential_holds_exactly_its_declared_capabilities() -> Result {
    scenario::run("runtime-credentials", async |scenario| {
        let pair = Pair::load(
            &scenario.inputs.initial_release,
            &scenario.inputs.successor_release,
        )?;
        exercise(scenario, &pair).await
    })
    .await
}

async fn exercise(scenario: &mut Scenario<'_>, pair: &Pair) -> Result {
    let checkpoint = Box::pin(deploy_with_pull_secret(scenario, pair)).await?;
    let objects = scenario.objects.clone();
    let node = scenario.node.clone();
    cnpg::wait_ready(&objects, WORKLOAD_NAMESPACE).await?;
    scenario.secret_canaries().await?;
    let workload = Workload {
        objects: &objects,
        namespace: WORKLOAD_NAMESPACE,
        node: &node,
        image: &pair.api_image,
    };
    let before = workload.identity(None, Duration::from_secs(300)).await?;
    let served = workload.messages(1, &before).await?;
    let (binding, sweep) = Box::pin(sweep_credential(scenario, pair, &checkpoint)).await?;

    note("(e) changing a key of the observed Secret");
    ensure(
        sweep.sensitive_revision,
        "the successor does not disclose the sensitive revision gap",
    )?;
    let probe = {
        use base64::Engine as _;
        let bytes = acceptance::random_bytes(24)?;
        scenario.recorder.canary(&bytes);
        base64::engine::general_purpose::STANDARD.encode(bytes)
    };
    scenario.recorder.canary(probe.as_bytes());
    let reaction = Box::pin(workload.secret_reaction(&before, &probe)).await?;
    scenario.recorder.check(
        "e-sensitive-revision-change-observed-and-removed",
        &json!({"gap": credentials::SENSITIVE_REVISION, "before": before, "served": served, "reaction": reaction}),
    );

    note("(f) checking the runtime binding is unchanged");
    ensure(
        credentials::binding(&objects, &checkpoint).await? == binding,
        "the proxy binding changed during the Secret change",
    )?;
    scenario
        .recorder
        .check("f-runtime-binding-unchanged", &json!({"binding": binding}));
    scenario.controller_logs("runtime-credentials").await?;
    nothing_leaks(scenario, "g-token-secret-and-rollout-key-canary-scan").await
}

/// Deploy the successor under a policy naming [`PULL_SECRET`], and check
/// (a); returns the committed generation's writer checkpoint.
async fn deploy_with_pull_secret(
    scenario: &mut Scenario<'_>,
    pair: &Pair,
) -> Result<WriterCheckpoint> {
    let successor = &pair.successor;
    let mut policy = pair.successor_policy.clone();
    policy.image_pull_secrets = vec![PULL_SECRET.to_owned()];
    let policies = BTreeMap::from([(DOMAIN.to_owned(), policy)]);
    let platform = scenario.prepare(&[successor], &policies).await?;
    let objects = scenario.objects.clone();
    create_pull_secret(&objects).await?;
    // One row per response: the Secret reaction reads the API back expecting
    // exactly the first row.
    objects
        .create(&json!({
            "apiVersion": "v1", "kind": "ConfigMap",
            "metadata": {"name": "api-settings", "namespace": WORKLOAD_NAMESPACE},
            "data": {"PGRST_DB_MAX_ROWS": "1"},
        }))
        .await?;
    note("deploying the successor");
    let run = scenario
        .deploy(&platform, &successor.root, "deploy-successor")
        .await?;
    let committed = aps::committed(
        &run,
        &successor.release_digest,
        &successor.manifest.ir_digest,
    )?;
    let checkpoint = writer_checkpoint(&platform.registry().await?, DOMAIN)?.clone();
    note("(a) checking the staged writers carry the policy's pull secret");
    let pulls = pull_secrets(&objects, &checkpoint).await?;
    scenario.recorder.check(
        "a-writers-carry-policy-image-pull-secrets",
        &json!({"report": committed_report(&committed), "writers": pulls}),
    );
    Ok(checkpoint)
}

/// (b), (c) and (d): read and review the proxy's token, sweep it, and
/// review the same requests with `SubjectAccessReview`.
async fn sweep_credential(
    scenario: &mut Scenario<'_>,
    pair: &Pair,
    checkpoint: &WriterCheckpoint,
) -> Result<(Binding, credentials::Sweep)> {
    let objects = scenario.objects.clone();
    let plan = &pair.successor.plan;
    note("(b) reviewing the proxy's projected token");
    let binding = credentials::binding(&objects, checkpoint).await?;
    let token = credentials::projected_token(&objects, &binding).await?;
    scenario.recorder.canary(token.as_bytes());
    ensure(
        credentials::binding(&objects, checkpoint).await? == binding,
        "the proxy binding changed while its token was read",
    )?;
    let (authenticated, expires_in) = token_bound(&objects, &binding, &token).await?;
    scenario.recorder.check(
        "b-projected-token-bound-to-the-proxy-pod",
        &json!({"binding": binding, "authenticated": authenticated, "expiresInSeconds": expires_in}),
    );

    note("(c) sweeping the runtime credential");
    let live = credentials::updated_objects(&objects, plan).await?;
    let sweep = credentials::sweep(plan, &live)?;
    let statuses = {
        let client = scenario.token_client(&token).await?;
        credentials::statuses(&client, &sweep).await?
    };
    drop(token);
    let requests = sweep
        .probes
        .iter()
        .zip(&statuses)
        .map(|(probe, status)| {
            json!({"request": probe, "status": status, "passed": *status == probe.expected})
        })
        .collect::<Vec<_>>();
    scenario.recorder.document(
        "runtime-credential-requests.json",
        serde_json::to_vec_pretty(&requests)?,
    );
    let failed = requests
        .iter()
        .filter(|request| request["passed"] != true)
        .collect::<Vec<_>>();
    ensure(
        failed.is_empty() && statuses.len() == sweep.probes.len(),
        format!("runtime credential requests did not behave as declared: {failed:?}"),
    )?;
    scenario.recorder.check(
        "c-declared-capabilities-gaps-and-denials",
        &json!({
            "requests": requests.len(),
            "allowed": sweep.probes.iter().filter(|probe| probe.allowed()).count(),
            "denied": sweep.probes.iter().filter(|probe| !probe.allowed()).count(),
            "demonstratedGaps": sweep.demonstrated,
            "declaredGaps": plan.enforcement_gaps,
        }),
    );

    note("(d) reviewing the same requests with SubjectAccessReview");
    let answers = credentials::review_access(objects.client(), &authenticated, &sweep).await?;
    let differing = answers
        .iter()
        .filter(|(_, expected, answer)| expected != answer)
        .map(|(access, expected, _)| json!({"access": access, "expectedAllowed": expected}))
        .collect::<Vec<_>>();
    ensure(
        differing.is_empty(),
        format!("SubjectAccessReview disagrees with the declared scope: {differing:?}"),
    )?;
    scenario.recorder.check(
        "d-subject-access-reviews-match",
        &json!({
            "reviews": answers.len(),
            "allowed": answers.iter().filter(|(_, expected, _)| *expected).count(),
        }),
    );
    ensure(
        credentials::binding(&objects, checkpoint).await? == binding,
        "the proxy binding changed during the sweep",
    )?;
    Ok((binding, sweep))
}

/// A registry credential for a registry nothing pulls from.
async fn create_pull_secret(objects: &Objects) -> Result {
    use base64::Engine as _;
    let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    let password = acceptance::workload::hex(&acceptance::random_bytes(16)?);
    let config = json!({"auths": {"registry.invalid": {"auth": encode(format!("acceptance:{password}").as_bytes())}}});
    objects
        .create(&json!({
            "apiVersion": "v1", "kind": "Secret", "type": "kubernetes.io/dockerconfigjson",
            "metadata": {"name": PULL_SECRET, "namespace": CONTROL_NAMESPACE},
            "data": {".dockerconfigjson": encode(config.to_string().as_bytes())},
        }))
        .await?;
    Ok(())
}

/// (a) The generation's controller and proxy Deployments, found by the UIDs
/// the writer checkpoint records, and the proxy Pod, each name exactly
/// [`PULL_SECRET`].
async fn pull_secrets(objects: &Objects, checkpoint: &WriterCheckpoint) -> Result<Value> {
    let expected = json!([{"name": PULL_SECRET}]);
    let deployments = objects
        .list("apps/v1", "Deployment", Some(CONTROL_NAMESPACE), None)
        .await?;
    let mut writers = Vec::new();
    for (role, uid) in [
        ("controller", checkpoint.controller_deployment_uid.as_str()),
        ("proxy", checkpoint.proxy_identity.deployment_uid.as_str()),
    ] {
        let [deployment] = deployments
            .iter()
            .filter(|item| text(item, "/metadata/uid") == uid)
            .collect::<Vec<_>>()[..]
        else {
            return Err(Error::Check(format!(
                "the {role} Deployment the checkpoint records is absent"
            )));
        };
        let secrets = &deployment["spec"]["template"]["spec"]["imagePullSecrets"];
        ensure(
            *secrets == expected,
            format!("the {role} Deployment carries {secrets} rather than the policy's pull secret"),
        )?;
        writers.push(json!({"role": role, "deployment": deployment["metadata"]["name"], "imagePullSecrets": secrets}));
    }
    let pod = objects
        .require(
            "v1",
            "Pod",
            Some(CONTROL_NAMESPACE),
            &checkpoint.proxy_identity.pod_name,
        )
        .await?;
    ensure(
        pod["spec"]["imagePullSecrets"] == expected,
        "the proxy Pod does not carry the policy's pull secret",
    )?;
    writers.push(json!({"role": "proxy-pod", "pod": pod["metadata"]["name"], "imagePullSecrets": pod["spec"]["imagePullSecrets"]}));
    Ok(Value::Array(writers))
}

/// (b) The token authenticates as the binding's `ServiceAccount`, bound to
/// its Pod, and expires within the projection's lifetime. Returns the
/// authenticated identity and the seconds the token has left.
async fn token_bound(
    objects: &Objects,
    binding: &Binding,
    token: &str,
) -> Result<(Authenticated, i64)> {
    let authenticated = credentials::review_token(objects.client(), token).await?;
    ensure(
        authenticated.username == binding.username()
            && authenticated.uid == binding.service_account_uid
            && authenticated.pod_name == binding.pod_name
            && authenticated.pod_uid == binding.pod_uid
            && authenticated
                .groups
                .contains(&format!("system:serviceaccounts:{}", binding.namespace)),
        format!("the TokenReview authenticates {authenticated:?}, not the proxy's binding"),
    )?;
    let claims = credentials::claims(token)?;
    let kubernetes = &claims["kubernetes.io"];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| Error::Infrastructure(error.to_string()))?
        .as_secs();
    let now = i64::try_from(now).map_err(|error| Error::Infrastructure(error.to_string()))?;
    let expires_in = claims["exp"].as_i64().unwrap_or_default() - now;
    ensure(
        text(kubernetes, "/namespace") == binding.namespace
            && text(kubernetes, "/serviceaccount/name") == binding.service_account_name
            && text(kubernetes, "/serviceaccount/uid") == binding.service_account_uid
            && text(kubernetes, "/pod/name") == binding.pod_name
            && text(kubernetes, "/pod/uid") == binding.pod_uid
            && expires_in > TOKEN_MARGIN_SECONDS
            && expires_in <= binding.token_lifetime_seconds + 60,
        format!(
            "the projected token's claims do not bind the proxy Pod within its lifetime (expires in {expires_in} s)"
        ),
    )?;
    Ok((authenticated, expires_in))
}
