"""Reject incomplete and cross-subject archival cleanup evidence before packaging."""
import io
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import package_cnpg_cleanup as evidence


class CleanupEvidenceChecks(unittest.TestCase):
    def fixture(self, scenario="finalizer"):
        digest = "sha256:" + "a" * 64
        pair = {key: digest for key in evidence.cleanup.demo.PAIR_DIGESTS}
        common = {"schemaVersion": evidence.cleanup.demo.SCHEMA, "candidates": pair}
        target = {"apiGroup": "", "resource": "configmaps", "namespace": "default", "name": "example"}
        def step(outcome, phase=None):
            fields = evidence.cleanup.demo.FIELDS.get(outcome, evidence.cleanup.demo.REGISTRY)
            role = "initial" if outcome.startswith("initial-") else "successor"
            values = {key: "metadata" for key in fields}
            values.update({key: value for key, value in {
                "transition": role + "-transition", "generation": role + "-generation", "phase": phase,
                "registryUid": "registry", "requestDigest": digest, "serviceAccountName": role + "-account",
                "creationTargets": [target], "managedTargets": [target],
                "creationTargetsSource": "authorized-candidate-policy-projection",
            }.items() if key in fields})
            if outcome == "verified-pair":
                values = {**evidence.cleanup.demo.EXPECTED_SCOPE, "initialCapabilityCount": 2, "successorCapabilityCount": 1}
            return {**common, "outcome": outcome, **values}
        steps = [step("verified-pair"), step("bootstrapped"), step("initial-prepared"), step("initial-reserved", "Planned"),
                 step("initial-staged", "Staged"), step("initial-activated", "Committed"), step("initial-inventory-published"),
                 step("successor-prepared"), step("successor-reserved", "Planned"), step("successor-staged", "Staged"),
                 step("upgrade-quiesced", "Quiesced"), step("upgrade-activated", "Cleaning")]
        if scenario == "finalizer":
            observed_step = step("upgrade-commit-pending", "Cleaning")
            steps.extend([observed_step, step("upgrade-committed", "Committed"),
                          step("successor-inventory-published"), step("successor-resolved", "Committed")])
            observed = {"check": "finalizer-pending-then-last-type-cleanup", "passed": True, "originalUid": "old-uid",
                        "finalizer": evidence.cleanup.FINALIZER, "pending": observed_step}
        else:
            observed_step = step("cleanup-uid-rejected", "Cleaning")
            steps.append(observed_step)
            observed = {"check": "replacement-uid-rejected-with-original-claim-retained", "passed": True,
                        "originalUid": "old-uid", "replacementUid": "new-uid", "rejection": observed_step}
        steps.append(step("verified-pair"))
        return {"schemaVersion": "apsides.dev/cnpg-cleanup-measurement/v1", "scenario": scenario,
                "candidatePair": pair, "purpose": evidence.cleanup.CnpgCleanupRunner.purpose,
                "runnerSourceCommit": evidence.PRODUCER_COMMIT,
                "frozenInputDigests": {"producer/source-archive": evidence.PRODUCER_ARCHIVE}, "policyDigest": digest,
                "checks": [{"check": "cnpg-ready-and-secret-bound", "clusterUid": "cluster", "secretUid": "secret",
                            "uriKeyPresent": True, "passed": True}, observed,
                           {"check": "unclaimed-same-type-resource-preserved", "passed": True, "uid": "control-uid",
                            "identity": evidence.cleanup.TARGET | {"name": evidence.cleanup.CONTROL_NAME}}],
                "runtimeRoles": [{"name": "successor-account", "uid": "role-uid", "rulesDigest": digest}],
                "steps": steps, "passed": True, "clusterDeleted": True, "credentialsRemoved": True,
                "releaseVerified": False, "productionEvidenceStatus": "not-produced",
                "limitation": "live measurement only; no production cleanup receipt contract"}

    def test_scenarios_produce_only_their_observed_checks(self):
        self.assertEqual(set(evidence.validate_measurement(self.fixture())),
                         {"last-api-type-cleanup", "finalizer-preservation"})
        self.assertEqual(set(evidence.validate_measurement(self.fixture("replacement"))), {"uid-replacement-rejection"})

    def test_rejects_unsupported_producer_or_claimed_release_acceptance(self):
        for key, value in [("runnerSourceCommit", "b" * 40), ("releaseVerified", True), ("passed", False),
                           ("clusterDeleted", False), ("credentialsRemoved", False), ("productionEvidenceStatus", "published")]:
            with self.subTest(key=key):
                measurement = self.fixture()
                measurement[key] = value
                with self.assertRaises(evidence.shared.QualificationError):
                    evidence.validate_measurement(measurement)

    def test_rejects_missing_commit_and_out_of_order_activation(self):
        value = self.fixture()
        value["steps"] = [step for step in value["steps"] if step["outcome"] != "upgrade-committed"]
        with self.assertRaises(evidence.shared.QualificationError):
            evidence.validate_measurement(value)
        value = self.fixture()
        value["steps"][10], value["steps"][11] = value["steps"][11], value["steps"][10]
        with self.assertRaises(evidence.shared.QualificationError):
            evidence.validate_measurement(value)

    def test_rejects_cross_candidate_or_transition_steps(self):
        for field, changed in [("candidates", {}), ("transition", "other"), ("registryUid", "other"), ("generation", "other")]:
            value = self.fixture()
            value["steps"][11] = {**value["steps"][11], field: changed}
            with self.subTest(field=field), self.assertRaises(evidence.shared.QualificationError):
                evidence.validate_measurement(value)

    def test_rejects_forged_pending_and_forced_finalizer(self):
        value = self.fixture()
        value["checks"][1]["pending"] = dict(value["checks"][1]["pending"], registryResourceVersion="other")
        with self.assertRaises(evidence.shared.QualificationError):
            evidence.validate_measurement(value)
        value = self.fixture()
        value["checks"][1]["finalizer"] = "some-other-finalizer"
        with self.assertRaises(evidence.shared.QualificationError):
            evidence.validate_measurement(value)

    def test_rejects_same_uid_and_wrong_rejection_phase(self):
        value = self.fixture("replacement")
        value["checks"][1]["replacementUid"] = "old-uid"
        with self.assertRaises(evidence.shared.QualificationError):
            evidence.validate_measurement(value)
        value = self.fixture("replacement")
        value["steps"][-2]["phase"] = "Committed"
        with self.assertRaises(evidence.shared.QualificationError):
            evidence.validate_measurement(value)

    def test_rejects_missing_control_and_wrong_runtime_role(self):
        value = self.fixture()
        value["checks"][2]["passed"] = False
        with self.assertRaises(evidence.shared.QualificationError):
            evidence.validate_measurement(value)
        value = self.fixture()
        value["runtimeRoles"][0]["name"] = "initial-account"
        with self.assertRaises(evidence.shared.QualificationError):
            evidence.validate_measurement(value)

    def test_archive_pin_prevents_self_described_producer_replacement(self):
        with self.assertRaises(evidence.shared.QualificationError):
            evidence.archive_sources(b"changed archive", {"producer/source-archive": evidence.shared.digest_bytes(b"changed archive")})

    def test_archive_sources_reject_symlinks_duplicate_members_and_digest_drift(self):
        def archive(symlink=False, duplicate=False):
            stream = io.BytesIO()
            with tarfile.open(fileobj=stream, mode="w") as output:
                member = tarfile.TarInfo("runner.py")
                if symlink:
                    member.type = tarfile.SYMTYPE
                    member.linkname = "/etc/passwd"
                    output.addfile(member)
                else:
                    member.size = 4
                    output.addfile(member, io.BytesIO(b"code"))
                    if duplicate:
                        output.addfile(member, io.BytesIO(b"code"))
            return stream.getvalue()
        for symlink, duplicate, digest in [(True, False, b"code"), (False, True, b"code"), (False, False, b"wrong")]:
            raw = archive(symlink, duplicate)
            with patch.object(evidence, "PRODUCER_ARCHIVE", evidence.shared.digest_bytes(raw)), \
                    patch.object(evidence, "SOURCES", {"runner.py": "runner.py"}), \
                    self.assertRaises(evidence.shared.QualificationError):
                evidence.archive_sources(raw, {"producer/runner.py": evidence.shared.digest_bytes(digest)})

    def test_duplicate_json_keys_and_symlink_inputs_are_rejected(self):
        with self.assertRaises(evidence.shared.QualificationError):
            evidence.parse(b'{"passed": true, "passed": false}')
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "real").write_text("safe")
            (root / "link").symlink_to("real")
            with self.assertRaises(evidence.shared.QualificationError):
                evidence.bounded(root / "link")


if __name__ == "__main__":
    unittest.main()
