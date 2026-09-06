//! Read-only list-decoding probe, not SSA or credential-scope qualification.
#![cfg(feature = "test-support")]

use kube::{Client, Config, config::KubeConfigOptions};
use platform::preview_transport::{PreviewTarget, PreviewTransport};

#[tokio::test]
#[ignore = "requires APS_PREVIEW_CONTEXT selecting an explicit live Kubernetes context"]
async fn preview_readonly_list_inherits_item_type_from_exact_list()
-> Result<(), Box<dyn std::error::Error>> {
    let context = std::env::var("APS_PREVIEW_CONTEXT")?;
    if context.is_empty() {
        return Err("live preview probe requires an explicit context".into());
    }
    platform::install_rustls_ring_provider()?;
    let mut config = Config::from_kubeconfig(&KubeConfigOptions {
        context: Some(context),
        ..KubeConfigOptions::default()
    })
    .await?;
    if config.cluster_url.scheme_str() != Some("https") || config.accept_invalid_certs {
        return Err("live preview probe requires verified TLS".into());
    }
    config.default_retry = false;
    let transport = PreviewTransport::new(Client::try_from(config)?);
    // The default namespace's root CA certificate is public cluster material.
    // Never print its body or turn this probe into a workload mutation.
    let target = PreviewTarget::new(
        "v1",
        "ConfigMap",
        "",
        "configmaps",
        "default",
        "kube-root-ca.crt",
    )?;
    let observation = transport.list_named(&target).await?;
    let object = observation
        .object()
        .ok_or("public root CA ConfigMap was absent")?;
    let types = object
        .types
        .as_ref()
        .ok_or("list item type was not established")?;
    assert_eq!(types.api_version, "v1");
    assert_eq!(types.kind, "ConfigMap");
    Ok(())
}
