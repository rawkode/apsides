"""Backup verifier mount isolation and closed receipt checks."""
import json
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from run_storage_backup import BACKUP_PVC, BackupRunner, DIGESTS, parse_backup_event


NONCE = "0123456789abcdef0123456789abcdef"


class BackupTests(unittest.TestCase):
    def test_verifier_has_only_read_only_backup_and_creator_has_two_mounts(self):
        runner = BackupRunner.__new__(BackupRunner)
        runner.image = {"reference": "localhost/test@sha256:" + "a" * 64}
        runner.node = "owned-control-plane"
        runner.nonce = NONCE
        verify = runner.pod_object("verify", "backup-verify", read_only=True)["spec"]
        self.assertEqual(verify["volumes"], [{"name": "store", "persistentVolumeClaim": {
            "claimName": BACKUP_PVC, "readOnly": True}}])
        self.assertEqual(verify["containers"][0]["volumeMounts"], [
            {"name": "store", "mountPath": "/store", "readOnly": True}])
        create = runner.pod_object("create", "backup-create", read_only=True)["spec"]
        self.assertEqual(len(create["volumes"]), 2)
        self.assertTrue(create["volumes"][0]["persistentVolumeClaim"]["readOnly"])
        self.assertFalse(create["volumes"][1]["persistentVolumeClaim"]["readOnly"])
        self.assertEqual(create["containers"][0]["volumeMounts"][1]["mountPath"], "/backup")

    def test_receipt_binds_original_and_later_refs_and_exact_revision(self):
        record = {key: "sha256:" + format(i, "064x") for i, key in enumerate(sorted(DIGESTS), 1)}
        record.update(schemaVersion="apsides.dev/pvc-coordination-backup/v1", nonce=NONCE,
                      phase="backup-verify", outcome="verified",
                      expectedInventoryRevision="2", observedInventoryRevision="2")
        def output(value):
            return ("APS_COORDINATION_BACKUP_RESULT=" + json.dumps(value)
                    + "\ntest pvc_coordination_phase ... ok\n"
                    + "test result: ok. 1 passed; 0 failed; 0 ignored\n")
        self.assertEqual(parse_backup_event(output(record), NONCE, "backup-verify"), record)
        for mutation in [{"observedInventoryRevision": "3"}, {"nonce": "f" * 32},
                         {"extra": "value"}, {"laterAuditDigest": record["seedAuditDigest"]}]:
            with self.subTest(mutation=mutation), self.assertRaises(RuntimeError):
                parse_backup_event(output(dict(record, **mutation)), NONCE, "backup-verify")


if __name__ == "__main__":
    unittest.main()
