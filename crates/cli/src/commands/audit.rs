//! Read-only transition history. Audit output never grants lifecycle authority.
use crate::{Result, cli, report};
use kube::{Client, Config};
use platform::PlatformClient;
use serde_json::{Value, json};
use std::time::Duration;

/// # Errors
/// Returns an error if the endpoint is not authenticated TLS, if the domain is
/// absent from the protected registry, or if the lookup times out — in which
/// case nothing is concluded about whether the transition exists.
pub fn command(arguments: &cli::Audit) -> Result<()> {
    let cli::Audit {
        domain,
        transition,
        cluster_binding,
        namespace,
        control_namespace,
    } = arguments;
    platform::install_rustls_ring_provider()?;
    let runtime = tokio::runtime::Runtime::new()?;
    let status = runtime.block_on(async {
        let mut config = Config::infer().await?;
        if config.cluster_url.scheme_str() != Some("https") || config.accept_invalid_certs {
            return Err("audit requires an authenticated TLS Kubernetes endpoint".into());
        }
        config.default_retry = false;
        config.connect_timeout = Some(Duration::from_secs(10));
        config.read_timeout = Some(Duration::from_secs(30));
        let store = crate::connection::connect_audit_reader().await?;
        let client = PlatformClient::new(
            Client::try_from(config)?,
            control_namespace,
            namespace,
            cluster_binding,
        )?
        .with_claim_store(store.clone())
        .with_audit_store(store);
        tokio::time::timeout(Duration::from_secs(30), lookup(&client, domain, transition))
            .await
            .map_err(|_| "audit lookup timed out; no conclusion about transition existence")?
    })?;
    report::emit(&status)
}

async fn lookup(client: &PlatformClient, domain: &str, transition: &str) -> Result<Value> {
    let registry = client.load_registry().await?;
    if !registry.value.histories.contains_key(domain) {
        return Err("audit domain is not present in the protected registry".into());
    }
    if let Some(stored) = registry.value.transitions.get(transition)
        && stored.domain_id() == domain
    {
        return Ok(retained_report(
            &registry.uid,
            &registry.resource_version,
            domain,
            transition,
            &stored.allocation().generation_id,
            stored.record.phase,
            &stored.record.executor_id,
        ));
    }
    let audit = client.load_transition_audit(domain, transition).await?;
    retired_report(domain, transition, audit)
}

fn retained_report(
    registry_uid: &str,
    registry_resource_version: &str,
    domain: &str,
    transition: &str,
    generation: &str,
    phase: deploy::Phase,
    executor: &str,
) -> Value {
    json!({
        "schemaVersion":"apsides.dev/transition-status/v1", "status":"retained",
        "registryUid":registry_uid,"registryResourceVersion":registry_resource_version,
        "domain":domain,"transition":transition,"generation":generation,
        "phase":phase,"executor":executor,"authority":false,
    })
}

fn retired_report(
    domain: &str,
    transition: &str,
    audit: Option<coordination::TransitionAuditV1>,
) -> Result<Value> {
    let audit =
        audit.ok_or("transition is not retained or archived in the protected audit chain")?;
    Ok(json!({
        "schemaVersion":"apsides.dev/transition-status/v1", "status":"retired",
        "domain":domain,"transition":transition,"audit":audit,"authority":false,
    }))
}

#[cfg(test)]
mod tests {
    use super::{retained_report, retired_report};
    use coordination::{
        AuditAllocation, AuditEvent, AuditEventKind, AuditScope, ClaimScope,
        TRANSITION_AUDIT_SCHEMA, TransitionAuditV1,
    };
    use deploy::Phase;
    use serde_json::json;

    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

    const REGISTRY_UID: &str = "11111111-1111-4111-8111-111111111111";

    fn audit() -> TestResult<TransitionAuditV1> {
        let names = deploy::allocation_names(REGISTRY_UID, 1)?;
        let events = [
            (AuditEventKind::Staged, "staged"),
            (AuditEventKind::Quiesced, "quiesced"),
            (AuditEventKind::Activated, "activated"),
            (AuditEventKind::Cleaning, "cleaning"),
            (AuditEventKind::Committed, "committed"),
        ]
        .into_iter()
        .map(|(kind, event_id)| AuditEvent {
            kind,
            event_id: event_id.to_owned(),
            verification_epoch: 0,
        })
        .collect::<Vec<_>>();
        let audit = TransitionAuditV1 {
            schema_version: TRANSITION_AUDIT_SCHEMA.to_owned(),
            scope: AuditScope {
                claim_scope: ClaimScope {
                    registry_uid: REGISTRY_UID.to_owned(),
                    cluster_binding: "cluster-a".to_owned(),
                    control_namespace: "control".to_owned(),
                    workload_namespace: "workload".to_owned(),
                },
                domain_id: "example".to_owned(),
                transition_id: names.transition_id.clone(),
                sequence: 1,
            },
            allocation: AuditAllocation {
                sequence: 1,
                generation_id: names.generation_id,
                transition_id: names.transition_id,
                service_account_name: names.service_account_name,
            },
            executor_id: "executor-a".to_owned(),
            final_revision: events.len() as u64,
            final_verification_epoch: 0,
            events,
            cleanup: Vec::new(),
            ownership: Vec::new(),
            retirement: Vec::new(),
            previous: None,
        };
        audit.validate()?;
        Ok(audit)
    }

    #[test]
    fn retained_output_is_explicitly_non_authoritative() {
        assert_eq!(
            retained_report(
                REGISTRY_UID,
                "43",
                "example",
                "transition-a",
                "generation-a",
                Phase::Committed,
                "executor-a",
            ),
            json!({
                "schemaVersion":"apsides.dev/transition-status/v1","status":"retained",
                "registryUid":REGISTRY_UID,"registryResourceVersion":"43",
                "domain":"example","transition":"transition-a","generation":"generation-a",
                "phase":"Committed","executor":"executor-a","authority":false,
            })
        );
    }

    #[test]
    fn retired_output_contains_the_sanitized_audit_and_no_authority() -> TestResult {
        let audit = audit()?;
        let report = retired_report("example", &audit.scope.transition_id, Some(audit.clone()))?;
        assert_eq!(report["status"], "retired");
        assert_eq!(report["authority"], false);
        assert_eq!(report["domain"], "example");
        assert_eq!(report["transition"], audit.scope.transition_id);
        assert_eq!(report["audit"], serde_json::to_value(audit)?);
        Ok(())
    }

    #[test]
    fn unknown_transition_is_not_reported_as_retired() {
        let error = retired_report("example", "transition-missing", None)
            .err()
            .map(|error| error.to_string());
        assert_eq!(
            error.as_deref(),
            Some("transition is not retained or archived in the protected audit chain")
        );
    }
}
