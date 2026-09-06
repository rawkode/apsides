use super::*;

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

#[test]
fn only_exact_new_phase_produces_one_metadata_only_checkpoint() -> TestResult {
    let (registry, plan, _, _) = crate::tests::preview_pending_fixture()?;
    let transition = &plan.request.transition_id;
    let previous = Versioned {
        uid: registry.registry_uid.clone(),
        resource_version: "42".into(),
        value: registry,
    };
    for (old, new) in [
        (None, Phase::Planned),
        (Some(Phase::Planned), Phase::Staged),
        (Some(Phase::Staged), Phase::Quiesced),
        (Some(Phase::Quiesced), Phase::Activated),
        (Some(Phase::Activated), Phase::Cleaning),
        (Some(Phase::Cleaning), Phase::Committed),
    ] {
        let pause = PhasePause::new(
            PauseAfterPhase {
                registry_uid: previous.uid.clone(),
                phase: new,
            },
            transition,
        )?;
        // This is a pure checkpoint-filter test. Wire tests separately establish
        // that the production persistence path validates records before observing.
        let mut before = previous.clone();
        let mut after = previous.clone();
        after.resource_version = "43".into();
        let entry = after
            .value
            .transitions
            .get_mut(transition)
            .ok_or("fixture transition")?;
        entry.record.phase = new;
        entry
            .staged_content_digests
            .insert("private-key-canary".into(), "private-value-canary".into());
        if let Some(phase) = old {
            before
                .value
                .transitions
                .get_mut(transition)
                .ok_or("fixture transition")?
                .record
                .phase = phase;
        } else {
            before.value.transitions.remove(transition);
        }
        let mut wrong = after.clone();
        wrong.uid = "different-registry".into();
        assert!(pause.checkpoint(&before, &wrong).is_none());
        wrong = after.clone();
        wrong.value.transitions.remove(transition);
        assert!(pause.checkpoint(&before, &wrong).is_none());
        assert!(pause.checkpoint(&after, &after).is_none());
        if new == Phase::Planned {
            assert!(pause.checkpoint(&previous, &after).is_none());
        }
        assert!(!pause.was_triggered());
        let checkpoint = pause
            .checkpoint(&before, &after)
            .ok_or("checkpoint missing")?;
        let value = serde_json::to_value(checkpoint)?;
        assert_eq!(
            value,
            serde_json::json!({
                "registryUid": previous.uid,
                "registryResourceVersion": "43",
                "transition": transition,
                "phase": new,
            })
        );
        assert!(!serde_json::to_string(&value)?.contains("canary"));
        assert!(pause.was_triggered());
        assert!(pause.checkpoint(&before, &after).is_none());
    }
    Ok(())
}

#[test]
fn malformed_or_blocked_checkpoint_configuration_is_rejected() -> TestResult {
    for value in [
        serde_json::json!({"registryUid":"uid", "phase":"Blocked"}),
        serde_json::json!({"registryUid":"", "phase":"Planned"}),
        serde_json::json!({"registryUid":"uid\n", "phase":"Planned"}),
    ] {
        assert!(PhasePause::new(serde_json::from_value(value)?, "transition").is_err());
    }
    assert!(
        serde_json::from_value::<PauseAfterPhase>(serde_json::json!({
            "registryUid":"uid", "phase":"Planned", "callback":"not-allowed",
        }))
        .is_err()
    );
    Ok(())
}
