//! Disposable-cluster credential/transport qualification, not signed-release/session preview.
use crate::{
    ApplyPrecondition, DeploymentIdentity, DeploymentPreviewApproval, DeploymentPreviewProfile,
    IssuedDeployment, Result, conditional_apply_request, invalid,
    preview_transport::{PreviewTarget, PreviewTransport},
};
use k8s_openapi::api::{
    core::v1::{ConfigMap, Namespace, Secret, ServiceAccount},
    rbac::v1::{Role, RoleBinding},
};
use kube::{
    Api, Client, Config, ResourceExt,
    api::{Patch, PatchParams, PostParams},
    client::Body,
    config::KubeConfigOptions,
};
use serde_json::{Value, json};

const CONTROL: &str = "aps-preview-control";
const WORKLOAD: &str = "aps-preview-workload";
const ACCOUNT: &str = "test-deployer";
const BINDING: &str = "test-deployer-binding";
const IDENTITY: &str = "reakt-deploy-identity-preview";
const MANAGER: &str = "reakt-preview";

#[tokio::test]
#[ignore = "requires an explicit disposable kind-aps-preview- context and KUBECONFIG"]
async fn issued_credentials_enforce_conditional_ssa_and_rbac() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(120), Box::pin(exercise()))
        .await
        .map_err(|_| invalid("issued preview live qualification timed out"))?
}

async fn exercise() -> Result<()> {
    let config = explicit_config().await?;
    let admin = Client::try_from(config.clone())
        .map_err(|_| invalid("live qualification admin client failed"))?;
    let version = admin
        .apiserver_version()
        .await
        .map_err(|_| invalid("live qualification version lookup failed"))?;
    if version.git_version != "v1.34.3" {
        return Err(invalid("live qualification requires Kubernetes v1.34.3"));
    }
    provision(&admin).await?;
    let issued =
        IssuedDeployment::connect(config, CONTROL, WORKLOAD, "preview-live", "preview").await?;
    issued.require_preview_approval().await?;
    let transport = PreviewTransport::new(issued.platform().client.clone());
    let version = issued
        .platform()
        .client
        .apiserver_version()
        .await
        .map_err(|_| invalid("issued version lookup failed"))?;
    assert_eq!(version.git_version, "v1.34.3");
    existing_dry_run(&admin, &transport, &issued.platform().client).await?;
    absent_dry_run(&admin, &transport).await?;
    denied_requests(&issued.platform().client).await?;
    Ok(())
}

async fn explicit_config() -> Result<Config> {
    let context = std::env::var("APS_PREVIEW_LIVE_CONTEXT")
        .map_err(|_| invalid("live qualification requires explicit context"))?;
    let path = std::env::var("KUBECONFIG")
        .map_err(|_| invalid("live qualification requires explicit KUBECONFIG"))?;
    if !context.starts_with("kind-aps-preview-")
        || context.len() <= "kind-aps-preview-".len()
        || !std::path::Path::new(&path).is_absolute()
        || !std::path::Path::new(&path).is_file()
        || path.contains(':')
    {
        return Err(invalid(
            "live qualification context or kubeconfig is invalid",
        ));
    }
    crate::install_rustls_ring_provider()?;
    let mut config = Config::from_kubeconfig(&KubeConfigOptions {
        context: Some(context),
        ..KubeConfigOptions::default()
    })
    .await
    .map_err(|_| invalid("live qualification kubeconfig failed"))?;
    if config.cluster_url.scheme_str() != Some("https") || config.accept_invalid_certs {
        return Err(invalid("live qualification requires verified TLS"));
    }
    config.default_retry = false;
    Ok(config)
}

async fn provision(admin: &Client) -> Result<()> {
    let namespaces: Api<Namespace> = Api::all(admin.clone());
    for name in [CONTROL, WORKLOAD] {
        let namespace = serde_json::from_value(json!({"metadata":{"name":name}}))
            .map_err(|_| invalid("namespace fixture invalid"))?;
        namespaces
            .create(&PostParams::default(), &namespace)
            .await
            .map_err(|_| invalid("disposable namespace creation failed"))?;
    }
    let accounts: Api<ServiceAccount> = Api::namespaced(admin.clone(), CONTROL);
    let account = accounts
        .create(
            &PostParams::default(),
            &serde_json::from_value(json!({
                "metadata":{"name":ACCOUNT},"automountServiceAccountToken":false
            }))
            .map_err(|_| invalid("account fixture invalid"))?,
        )
        .await
        .map_err(|_| invalid("disposable account creation failed"))?;
    let secrets: Api<Secret> = Api::namespaced(admin.clone(), CONTROL);
    let secret = secrets
        .create(
            &PostParams::default(),
            &serde_json::from_value(json!({
                "metadata":{"name":BINDING},"type":"Opaque","immutable":true,"data":{}
            }))
            .map_err(|_| invalid("binding fixture invalid"))?,
        )
        .await
        .map_err(|_| invalid("disposable binding creation failed"))?;
    let identity = DeploymentIdentity {
        schema_version: crate::DEPLOYMENT_IDENTITY_SCHEMA_V2.into(),
        domain_id: "preview".into(),
        cluster_binding: "preview-live".into(),
        service_account_name: ACCOUNT.into(),
        service_account_uid: account
            .uid()
            .ok_or_else(|| invalid("account UID missing"))?,
        binding_secret_name: BINDING.into(),
        binding_secret_uid: secret.uid().ok_or_else(|| invalid("binding UID missing"))?,
        audience: "https://kubernetes.default.svc.cluster.local".into(),
        lifetime_seconds: 600,
        preview: Some(DeploymentPreviewApproval {
            profile: DeploymentPreviewProfile::StaticSsaIssuedDeployerV1,
            targets: ["absent", "target"]
                .into_iter()
                .map(|name| coordination::ReviewIdentity {
                    api_group: String::new(),
                    resource: "configmaps".into(),
                    namespace: WORKLOAD.into(),
                    name: name.into(),
                })
                .collect(),
        }),
    };
    let state =
        serde_json::to_string(&identity).map_err(|_| invalid("identity encoding failed"))?;
    let identities: Api<ConfigMap> = Api::namespaced(admin.clone(), CONTROL);
    identities
        .create(
            &PostParams::default(),
            &serde_json::from_value(json!({
                "metadata":{"name":IDENTITY},"data":{"state.json":state}
            }))
            .map_err(|_| invalid("identity fixture invalid"))?,
        )
        .await
        .map_err(|_| invalid("disposable identity creation failed"))?;
    provision_roles(admin).await
}

async fn provision_roles(admin: &Client) -> Result<()> {
    for (namespace, rules) in [
        (
            CONTROL,
            json!([{"apiGroups":[""],"resources":["configmaps"],
            "resourceNames":[IDENTITY],"verbs":["get"]}]),
        ),
        (
            WORKLOAD,
            json!([
                {"apiGroups":[""],"resources":["configmaps"],
                    "resourceNames":["absent","target"],"verbs":["list","patch"]},
                {"apiGroups":[""],"resources":["configmaps"],"verbs":["create"]}
            ]),
        ),
    ] {
        let roles: Api<Role> = Api::namespaced(admin.clone(), namespace);
        roles
            .create(
                &PostParams::default(),
                &serde_json::from_value(json!({
                    "metadata":{"name":"preview-test"},"rules":rules
                }))
                .map_err(|_| invalid("role fixture invalid"))?,
            )
            .await
            .map_err(|_| invalid("disposable role creation failed"))?;
        let bindings: Api<RoleBinding> = Api::namespaced(admin.clone(), namespace);
        bindings.create(&PostParams::default(), &serde_json::from_value(json!({
            "metadata":{"name":"preview-test"},
            "roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"Role","name":"preview-test"},
            "subjects":[{"kind":"ServiceAccount","name":ACCOUNT,"namespace":CONTROL}]
        })).map_err(|_| invalid("role binding fixture invalid"))?).await
            .map_err(|_| invalid("disposable role binding creation failed"))?;
    }
    Ok(())
}

fn desired(name: &str, value: &str) -> Value {
    json!({"apiVersion":"v1","kind":"ConfigMap", "metadata":{"name":name,"namespace":WORKLOAD},
        "data":{"synthetic":value}})
}

fn target(name: &str) -> Result<PreviewTarget> {
    PreviewTarget::new("v1", "ConfigMap", "", "configmaps", WORKLOAD, name)
}

async fn existing_dry_run(
    admin: &Client,
    transport: &PreviewTransport,
    issued: &Client,
) -> Result<()> {
    let api: Api<ConfigMap> = Api::namespaced(admin.clone(), WORKLOAD);
    api.patch(
        "target",
        &PatchParams::apply(MANAGER),
        &Patch::Apply(desired("target", "before")),
    )
    .await
    .map_err(|_| invalid("synthetic target setup failed"))?;
    let before = api
        .get("target")
        .await
        .map_err(|_| invalid("synthetic target read failed"))?;
    let observation = transport.list_named(&target("target")?).await?;
    let object = observation
        .object()
        .ok_or_else(|| invalid("issued target list was empty"))?;
    let uid = object
        .metadata
        .uid
        .as_deref()
        .ok_or_else(|| invalid("target UID missing"))?;
    let rv = object
        .metadata
        .resource_version
        .as_deref()
        .ok_or_else(|| invalid("target RV missing"))?;
    let apply = conditional_apply_request(
        &desired("target", "after"),
        "target",
        WORKLOAD,
        MANAGER,
        false,
        ApplyPrecondition::Existing {
            uid,
            resource_version: rv,
        },
    )?;
    let projected = transport.dry_run_apply(&target("target")?, &apply).await?;
    assert!(matches!(
        projected
            .object()
            .data
            .pointer("/data/synthetic")
            .and_then(Value::as_str),
        Some("after")
    ));
    let after = api
        .get("target")
        .await
        .map_err(|_| invalid("synthetic target reread failed"))?;
    assert!(before == after, "dry-run changed the persisted target");
    reject_stale(transport, issued, uid, rv).await?;
    let final_state = api
        .get("target")
        .await
        .map_err(|_| invalid("synthetic final read failed"))?;
    assert!(
        before == final_state,
        "rejected dry-run changed the persisted target"
    );
    Ok(())
}

async fn reject_stale(
    transport: &PreviewTransport,
    issued: &Client,
    uid: &str,
    rv: &str,
) -> Result<()> {
    // Kubernetes validates UID immutability separately from optimistic concurrency.
    for (expected_uid, expected_rv, status) in [
        (
            "00000000-0000-0000-0000-000000000000",
            rv,
            http::StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (uid, "1", http::StatusCode::CONFLICT),
    ] {
        let apply = conditional_apply_request(
            &desired("target", "rejected"),
            "target",
            WORKLOAD,
            MANAGER,
            false,
            ApplyPrecondition::Existing {
                uid: expected_uid,
                resource_version: expected_rv,
            },
        )?;
        let request = kube::core::Request::new(format!("/api/v1/namespaces/{WORKLOAD}/configmaps"))
            .patch(
                "target",
                &apply.dry_run_params(),
                &Patch::Apply(apply.payload()),
            )
            .map_err(|_| invalid("stale conflict request invalid"))?;
        let response = issued
            .send(request.map(Body::from))
            .await
            .map_err(|_| invalid("stale conflict transport failed"))?;
        assert_eq!(response.status(), status);
        drop(response);
        assert!(
            transport
                .dry_run_apply(&target("target")?, &apply)
                .await
                .is_err(),
            "stale conditional dry-run unexpectedly succeeded"
        );
    }
    Ok(())
}

async fn absent_dry_run(admin: &Client, transport: &PreviewTransport) -> Result<()> {
    let observation = transport.list_named(&target("absent")?).await?;
    assert!(observation.object().is_none());
    let apply = conditional_apply_request(
        &desired("absent", "synthetic"),
        "absent",
        WORKLOAD,
        MANAGER,
        false,
        ApplyPrecondition::Absent {
            list_resource_version: observation.resource_version(),
        },
    )?;
    let projected = transport.dry_run_apply(&target("absent")?, &apply).await?;
    assert!(matches!(
        projected
            .object()
            .data
            .pointer("/data/synthetic")
            .and_then(Value::as_str),
        Some("synthetic")
    ));
    let api: Api<ConfigMap> = Api::namespaced(admin.clone(), WORKLOAD);
    let persisted = api
        .get_opt("absent")
        .await
        .map_err(|_| invalid("absent target read failed"))?;
    assert!(
        persisted.is_none(),
        "absent dry-run created a persisted object"
    );
    Ok(())
}

async fn denied_requests(client: &Client) -> Result<()> {
    for uri in [
        format!("/api/v1/namespaces/{CONTROL}/secrets/{BINDING}"),
        format!("/api/v1/namespaces/{WORKLOAD}/configmaps"),
        format!("/api/v1/namespaces/{WORKLOAD}/configmaps?fieldSelector=metadata.name%3Dother"),
    ] {
        let request = http::Request::get(uri)
            .body(Vec::new())
            .map_err(|_| invalid("denial request invalid"))?;
        require_forbidden(client, request).await?;
    }
    let request = http::Request::patch(format!(
        "/api/v1/namespaces/{WORKLOAD}/configmaps/other?dryRun=All&fieldManager={MANAGER}&fieldValidation=Strict"
    )).header("Content-Type", "application/apply-patch+yaml")
        .body(serde_json::to_vec(&desired("other", "denied"))
            .map_err(|_| invalid("denial payload encoding failed"))?)
        .map_err(|_| invalid("denial patch invalid"))?;
    require_forbidden(client, request).await
}

async fn require_forbidden(client: &Client, request: http::Request<Vec<u8>>) -> Result<()> {
    let response = client
        .send(request.map(Body::from))
        .await
        .map_err(|_| invalid("issued denial request transport failed"))?;
    // Discard the body without reading or printing Kubernetes diagnostics.
    assert!(
        response.status() == http::StatusCode::FORBIDDEN,
        "issued request was not denied with 403"
    );
    Ok(())
}
