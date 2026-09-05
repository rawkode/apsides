use controller_core::{AutomationRunCleanupV0, RecoveryProofV0};

#[test]
fn downstream_can_construct_recovery_and_cleanup_protocol_types() {
    let proof = RecoveryProofV0 {
        root_uid: "root".into(),
        transaction_id: "transaction".into(),
        command_id: "a".repeat(64),
        uid: "uid".into(),
        resource_version: "7".into(),
    };
    let cleanup = AutomationRunCleanupV0 {
        occurrence_id: "occurrence".into(),
        name: "run".into(),
        uid: "uid".into(),
        resource_version: "7".into(),
    };

    assert_eq!(proof.uid, cleanup.uid);
}
