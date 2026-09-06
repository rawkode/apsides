use super::*;
use crate::phase_interruption::{PauseAfterPhase, PhasePause};
use deploy::Phase;
use std::time::Duration;

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

#[tokio::test]
async fn checkpoint_parks_only_after_successful_exact_registry_commit() -> TestResult {
    for outcome in ["success", "conflict", "changed-response"] {
        let registry = audit_retention::prepare_cleaning_upgrade()?;
        let previous = Versioned {
            value: registry.registry.clone(),
            uid: REGISTRY_UID.into(),
            resource_version: "42".into(),
        };
        let response = match outcome {
            "success" => json!("echo-put"),
            "conflict" => json!({"wireHttpStatus":409,"body":{
                "apiVersion":"v1","kind":"Status","status":"Failure",
                "reason":"Conflict","message":"synthetic CAS conflict","code":409,
            }}),
            "changed-response" => registry_object(&registry, REGISTRY_UID, "43")?,
            _ => return Err("unexpected wire scenario".into()),
        };
        let policy = policy(&registry.plan(TRANSITION_B)?.request.next.compiled);
        let (client, calls) = wire(vec![
            object("reakt-policy-example", &policy, "policy-uid", "40"),
            registry_object(&registry, REGISTRY_UID, "42")?,
            response,
        ]);
        let mut platform =
            registry.bind(client, "control", "workload", "cluster-a", TRANSITION_B)?;
        let pause = Arc::new(PhasePause::new(
            PauseAfterPhase {
                registry_uid: REGISTRY_UID.into(),
                phase: Phase::Committed,
            },
            TRANSITION_B,
        )?);
        platform.phase_pause = Some(pause.clone());
        let event = audit_retention::committed_event("committed-b")?;
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            platform.transition_event(&previous, TRANSITION_B, "executor-b", event),
        )
        .await;
        let calls = calls.lock().map_err(|_| "calls lock poisoned")?;
        assert_eq!(
            calls.iter().filter(|call| call.0 == "PUT").count(),
            1,
            "{outcome}"
        );
        if outcome == "success" {
            assert!(result.is_err(), "validated commit must remain parked");
            assert!(pause.was_triggered());
        } else {
            let error = result
                .map_err(|_| "rejected CAS unexpectedly parked")?
                .err()
                .ok_or("rejected CAS unexpectedly succeeded")?;
            match outcome {
                "conflict" => assert!(
                    matches!(error, Error::Kubernetes(kube::Error::Api(status)) if status.code == 409)
                ),
                "changed-response" => assert!(
                    error
                        .to_string()
                        .contains("admission changed the protected publication candidate")
                ),
                _ => return Err("unexpected rejection scenario".into()),
            }
            assert!(!pause.was_triggered());
        }
    }
    Ok(())
}
