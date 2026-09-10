use super::*;
use crate::{DEPLOYMENT_IDENTITY_SCHEMA_V2, DeploymentPreviewApproval, DeploymentPreviewProfile};
use coordination::ReviewIdentity;
use http::{Request, Response, header};
use kube::{
    Client,
    client::Body,
    config::{AuthProviderConfig, ExecConfig, NamedExtension},
};
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tower::service_fn;

fn config(url: &str) -> Result<Config> {
    let uri = url
        .parse()
        .map_err(|_| invalid("issued deployment test URL is invalid"))?;
    Ok(Config::new(uri))
}

#[test]
fn issued_config_removes_inherited_identity_and_preserves_tls_endpoint() -> Result<()> {
    let mut config = config("https://api.example.invalid:6443")?;
    config.root_cert = Some(vec![b"root-one".to_vec(), b"root-two".to_vec()]);
    config.root_cert_file = Some(PathBuf::from("/trusted/cluster-ca.pem"));
    config.tls_server_name = Some("kubernetes.example.invalid".into());
    config.auth_info = synthetic_auth_info();
    config.headers.push((
        header::AUTHORIZATION,
        http::HeaderValue::from_static("Bearer inherited-header"),
    ));
    config.headers.push((
        http::HeaderName::from_static("x-private-canary"),
        http::HeaderValue::from_static("inherited-header-value"),
    ));
    let endpoint = config.cluster_url.clone();
    let roots = config.root_cert.clone();
    let root_file = config.root_cert_file.clone();
    let server_name = config.tls_server_name.clone();

    let issued = issued_config(config, "issued-token".into());
    assert_eq!(issued.cluster_url, endpoint);
    assert_eq!(issued.root_cert, roots);
    assert_eq!(issued.root_cert_file, root_file);
    assert_eq!(issued.tls_server_name, server_name);
    assert!(issued.headers.is_empty());
    assert_cleared_auth(&issued.auth_info);
    Ok(())
}

fn synthetic_auth_info() -> AuthInfo {
    let mut impersonate_user_extra = HashMap::new();
    impersonate_user_extra.insert("scope".into(), vec!["private-extra".into()]);
    let mut auth_provider_config = HashMap::new();
    auth_provider_config.insert("credential".into(), "private-provider".into());
    let mut auth_provider_other = BTreeMap::new();
    auth_provider_other.insert("privateProviderOther".into(), json!(true));
    let mut other = BTreeMap::new();
    other.insert("privateAuthOther".into(), json!(true));
    AuthInfo {
        username: Some("private-user".into()),
        password: Some("private-password".into()),
        token: Some("private-token".into()),
        token_file: Some("/private/token".into()),
        client_certificate: Some("/private/certificate".into()),
        client_certificate_data: Some("private-certificate-data".into()),
        client_key: Some("/private/key".into()),
        client_key_data: Some("private-key-data".into()),
        impersonate: Some("private-impersonation".into()),
        impersonate_uid: Some("private-impersonation-uid".into()),
        impersonate_groups: Some(vec!["private-group".into()]),
        impersonate_user_extra: Some(impersonate_user_extra),
        extensions: Some(vec![NamedExtension {
            name: "private-extension".into(),
            extension: json!({"credential":"private-extension-value"}),
        }]),
        auth_provider: Some(AuthProviderConfig {
            name: "private-provider".into(),
            config: auth_provider_config,
            other: auth_provider_other,
        }),
        exec: Some(ExecConfig {
            command: Some("private-exec".into()),
            args: Some(vec!["private-argument".into()]),
            ..ExecConfig::default()
        }),
        other,
    }
}

fn assert_cleared_auth(auth: &AuthInfo) {
    assert_eq!(
        auth.token.as_ref().map(ExposeSecret::expose_secret),
        Some("issued-token")
    );
    assert!(auth.username.is_none());
    assert!(auth.password.is_none());
    assert!(auth.token_file.is_none());
    assert!(auth.client_certificate.is_none());
    assert!(auth.client_certificate_data.is_none());
    assert!(auth.client_key.is_none());
    assert!(auth.client_key_data.is_none());
    assert!(auth.impersonate.is_none());
    assert!(auth.impersonate_uid.is_none());
    assert!(auth.impersonate_groups.is_none());
    assert!(auth.impersonate_user_extra.is_none());
    assert!(auth.extensions.is_none());
    assert!(auth.auth_provider.is_none());
    assert!(auth.exec.is_none());
    assert!(auth.other.is_empty());
}

#[test]
fn transport_configuration_requires_tls_and_fixes_retry_and_timeouts() -> Result<()> {
    let mut insecure = config("http://api.example.invalid")?;
    assert!(configure_transport(&mut insecure).is_err());
    let mut unverified = config("https://api.example.invalid")?;
    unverified.accept_invalid_certs = true;
    assert!(configure_transport(&mut unverified).is_err());

    let mut secure = config("https://api.example.invalid")?;
    secure.default_retry = true;
    secure.connect_timeout = None;
    secure.read_timeout = None;
    secure.write_timeout = None;
    configure_transport(&mut secure)?;
    assert!(!secure.default_retry);
    assert_eq!(secure.connect_timeout, Some(Duration::from_secs(10)));
    assert_eq!(secure.read_timeout, Some(Duration::from_secs(30)));
    assert_eq!(secure.write_timeout, Some(Duration::from_secs(30)));
    Ok(())
}

fn approval(targets: Vec<ReviewIdentity>) -> DeploymentPreviewApproval {
    DeploymentPreviewApproval {
        profile: DeploymentPreviewProfile::StaticSsaIssuedDeployerV1,
        targets,
    }
}

fn identity(preview: Option<DeploymentPreviewApproval>) -> DeploymentIdentity {
    DeploymentIdentity {
        schema_version: DEPLOYMENT_IDENTITY_SCHEMA_V2.into(),
        domain_id: "example".into(),
        cluster_binding: "cluster-a".into(),
        service_account_name: "deployer".into(),
        service_account_uid: "service-account-uid".into(),
        binding_secret_name: "binding".into(),
        binding_secret_uid: "binding-secret-uid".into(),
        audience: "https://kubernetes.default.svc".into(),
        lifetime_seconds: 600,
        preview,
    }
}

fn protected_identity(
    identity: &DeploymentIdentity,
    uid: &str,
    resource_version: &str,
) -> Result<Value> {
    let state = serde_json::to_string(identity)
        .map_err(|_| invalid("issued deployment test identity is invalid"))?;
    Ok(json!({
        "apiVersion":"v1",
        "kind":"ConfigMap",
        "metadata":{
            "name":"apsides-deploy-identity-example",
            "namespace":"control",
            "uid":uid,
            "resourceVersion":resource_version
        },
        "data":{"state.json":state}
    }))
}

fn platform_with_identity(
    identity: &DeploymentIdentity,
    uid: &str,
    resource_version: &str,
) -> Result<(PlatformClient, Arc<AtomicUsize>)> {
    let response = serde_json::to_vec(&protected_identity(identity, uid, resource_version)?)
        .map_err(|_| invalid("issued deployment test response is invalid"))?;
    let calls = Arc::new(AtomicUsize::new(0));
    let service = service_fn({
        let calls = calls.clone();
        move |_: Request<Body>| {
            calls.fetch_add(1, Ordering::Relaxed);
            let response = response.clone();
            async move { Ok::<_, std::convert::Infallible>(Response::new(Body::from(response))) }
        }
    });
    Ok((
        PlatformClient::new(
            Client::new(service, "control"),
            "control",
            "workload",
            "cluster-a",
        )?,
        calls,
    ))
}

fn issued_fixture(
    pinned: DeploymentIdentity,
    current: &DeploymentIdentity,
    current_uid: &str,
    current_resource_version: &str,
    expires: k8s_openapi::jiff::Timestamp,
) -> Result<(IssuedDeployment, Arc<AtomicUsize>)> {
    let (platform, calls) = platform_with_identity(current, current_uid, current_resource_version)?;
    Ok((
        IssuedDeployment {
            platform,
            identity: Versioned {
                value: pinned,
                uid: "identity-policy-uid".into(),
                resource_version: "1".into(),
            },
            expires_at: expires.to_string(),
            expires,
        },
        calls,
    ))
}

fn expiration(offset_seconds: i64) -> Result<k8s_openapi::jiff::Timestamp> {
    k8s_openapi::jiff::Timestamp::from_second(
        k8s_openapi::jiff::Timestamp::now().as_second() + offset_seconds,
    )
    .map_err(|_| invalid("issued deployment test expiration is invalid"))
}

#[tokio::test]
async fn missing_or_expired_preview_approval_performs_no_request() -> Result<()> {
    let without = identity(None);
    let (issued, calls) = issued_fixture(
        without.clone(),
        &without,
        "identity-policy-uid",
        "1",
        expiration(600)?,
    )?;
    assert!(issued.require_preview_approval().await.is_err());
    assert_eq!(calls.load(Ordering::Relaxed), 0);

    let with = identity(Some(approval(Vec::new())));
    let (issued, calls) = issued_fixture(
        with.clone(),
        &with,
        "identity-policy-uid",
        "1",
        expiration(-1)?,
    )?;
    assert!(issued.require_preview_approval().await.is_err());
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    Ok(())
}

#[tokio::test]
async fn changed_pinned_preview_identity_is_rejected() -> Result<()> {
    let pinned = identity(Some(approval(Vec::new())));
    let mut changed_body = pinned.clone();
    changed_body.audience = "changed-audience".into();
    let changes = [
        (changed_body, "identity-policy-uid", "1"),
        (pinned.clone(), "replacement-policy-uid", "1"),
        (pinned.clone(), "identity-policy-uid", "2"),
    ];
    for (current, uid, resource_version) in changes {
        let (issued, calls) = issued_fixture(
            pinned.clone(),
            &current,
            uid,
            resource_version,
            expiration(600)?,
        )?;
        assert!(issued.require_preview_approval().await.is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
    Ok(())
}

#[tokio::test]
async fn exact_preview_approval_rechecks_and_accepts_empty_or_named_targets() -> Result<()> {
    let target_sets = [
        Vec::new(),
        vec![ReviewIdentity {
            api_group: String::new(),
            resource: "configmaps".into(),
            namespace: "workload".into(),
            name: "settings".into(),
        }],
    ];
    for targets in target_sets {
        let identity = identity(Some(approval(targets.clone())));
        let (issued, calls) = issued_fixture(
            identity.clone(),
            &identity,
            "identity-policy-uid",
            "1",
            expiration(600)?,
        )?;
        let approved = issued.require_preview_approval().await?;
        assert_eq!(approved.targets, targets);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
    Ok(())
}
