#!/usr/bin/env python3
"""Offline contract checks; synthetic records are never passed to publication."""
import copy
import io
import tarfile
import types
from unittest.mock import patch
import unittest

import package_cnpg_phase_recovery as package

D = "sha256:" + "1" * 64


def measurement(phase):
    pair = {key: D for key in package.demo.PAIR_DIGESTS}
    order = ["verified-pair", "bootstrapped", "initial-prepared", "initial-reserved", "initial-staged", "initial-activated",
             "initial-inventory-published", "initial-resolved", "rejection-prepared", "upgrade-rejected", "successor-prepared",
             "successor-reserved", "successor-staged", "upgrade-quiesced", "upgrade-activated", "upgrade-committed",
             "successor-inventory-published", "successor-resolved", "upgrade-committed", "verified-pair"]
    phases = {"initial-reserved": "Planned", "initial-staged": "Staged", "initial-activated": "Committed",
              "initial-resolved": "Committed", "successor-reserved": "Planned", "successor-staged": "Staged",
              "upgrade-quiesced": "Quiesced", "upgrade-activated": "Cleaning", "upgrade-committed": "Committed", "successor-resolved": "Committed"}
    steps = []
    for outcome in order:
        step = {key: "value" for key in package.demo.FIELDS[outcome]}
        step.update(schemaVersion=package.demo.SCHEMA, outcome=outcome, candidates=pair)
        for key in step:
            if key in {"requestDigest", "missingCapabilityDigest"}: step[key] = D
            if key in {"initialCapabilityCount", "successorCapabilityCount"}: step[key] = 1
            if key == "registryUid": step[key] = "registry-" + phase
            if key == "registryResourceVersion": step[key] = "10"
            if key == "transition": step[key] = "t2" if outcome.startswith(("successor", "upgrade")) else "t1"
            if key == "generation": step[key] = "g2" if outcome.startswith(("successor", "upgrade")) else "g1"
            if key in {"creationTargets", "managedTargets"}: step[key] = [{"apiGroup": "", "resource": "configmaps", "namespace": "default", "name": "api-config"}]
            if key == "creationTargetsSource": step[key] = "authorized-candidate-policy-projection"
        if outcome in phases: step["phase"] = phases[outcome]
        if outcome == "verified-pair": step.update(package.demo.EXPECTED_SCOPE)
        steps.append(step)
    before = dict(registryUid="registry-" + phase, registryResourceVersion="10", transition="t2", generation="g2", phase=phase,
                  executorId="candidate-pair-qualification", executorBindingDigest=D, registryDigest=D)
    resumed = dict(before, phase="Cleaning" if phase == "Activated" else phase)
    final = dict(before, phase="Committed")
    completed = {"Planned": "successor-reserved", "Staged": "successor-staged", "Quiesced": "upgrade-quiesced",
                 "Activated": "upgrade-activated", "Cleaning": "upgrade-activated", "Committed": "upgrade-committed"}[phase]
    snapshot = {table: {"schemaDigest": D, "rowCount": 0, "rowDigests": []} for table in package.demo.base.observations.INVENTORY_TABLES}
    readback = phase in {"Planned", "Staged", "Quiesced"}
    marked = dict(before, stagedIdentityCount=0 if phase == "Planned" else 3)
    m = {"schemaVersion": "apsides.dev/cnpg-phase-recovery-measurement/v1", "passed": True, "phase": phase,
         "candidatePair": pair, "successorSubject": {"testSubject": D}, "runnerSourceCommit": "a" * 40,
         "sourceArchiveDigest": D, "runnerSourceDigest": D,
         "frozenInputDigests": {"producer/source-archive": D, "producer/phase-recovery-runner.py": D},
         "interruption": {"interruptedAction": package.runner.PHASES[phase],
                          "checkpoint": {k: before[k] for k in ["registryUid", "registryResourceVersion", "transition", "phase"]},
                          "protectedBindingBefore": marked, "protectedBindingAfterKill": copy.deepcopy(marked),
                          "termination": {"pid": 123, "signal": "SIGKILL", "returnCode": -9, "reaped": True},
                          "interruptedSession": 1, "resumeAction": "resume-checkpoint" if readback else package.runner.PHASES[phase],
                          "recoveryMode": "protected-checkpoint-readback" if readback else "operation-replay",
                          "resumedResult": next(s for s in steps if s["outcome"] == completed), "resumedBinding": resumed,
                          "freshIssuedSession": True, "resumedSession": 2},
         "finalCommit": final, "inventory": next(s for s in steps if s["outcome"] == "successor-inventory-published"),
         "inventoryIdempotence": {"before": snapshot, "after": copy.deepcopy(snapshot), "unchanged": True},
         "repeatedCommit": steps[-2], "oldWriterSafety": {"nonterminalOldWriterPods": 0, "terminalOldWriterPodUids": [], "retirementVerifiedByProductionActivation": True},
         "workloadChecks": [{"check": name, "passed": True} for name in ["cnpg-ready-and-secret-bound", "postgrest-messages-live", "configmap-driven-pod-turnover", "postgrest-messages-after-rollout", "secret-bound-before-successor-start"]],
         "steps": steps, "cluster": "cluster-" + phase, "clusterDeleted": True, "credentialsRemoved": True, "releaseVerified": False,
         "limitation": "one post-CAS owned process killed and reaped; same logical owner, no stalled-executor takeover"}
    checks = {x["check"]: x for x in m["workloadChecks"]}
    checks["cnpg-ready-and-secret-bound"].update(clusterUid="cnpg", secretUid="secret", uriKeyPresent=True)
    checks["postgrest-messages-live"].update(status=200, rowCount=2, bodyDigest=D)
    checks["postgrest-messages-after-rollout"].update(status=200, rowCount=1, bodyDigest=D)
    identity = dict(deploymentUid="deployment", deploymentResourceVersion="1", revision="revision1", replicaSetUid="rs1", podUid="pod1", containerImageId="image", selectedImageManifestDigest=D, restartCount=0)
    checks["configmap-driven-pod-turnover"].update(before=identity, after=dict(identity, revision="revision2", replicaSetUid="rs2", podUid="pod2"))
    checks["secret-bound-before-successor-start"]["claim"] = "binding-only; no Secret creation reaction claimed"
    return m


class PackagingTests(unittest.TestCase):
    def test_all_six_phases_validate_together(self):
        package.validate_set([measurement(p) for p in package.runner.PHASES])

    def reject(self, mutate, phase="Planned"):
        m = measurement(phase)
        mutate(m)
        with self.assertRaises((package.shared.QualificationError, RuntimeError)):
            package.validate_measurement(m)

    def test_missing_or_duplicate_phase_rejected(self):
        values = [measurement(p) for p in package.runner.PHASES]
        for invalid in [values[:5], [values[0]] * 6]:
            with self.assertRaises(package.shared.QualificationError): package.validate_set(invalid)

    def test_unreaped_or_unsignalled_child_rejected(self):
        for key, value in [("reaped", False), ("returnCode", 0), ("signal", "SIGTERM")]:
            self.reject(lambda m: m["interruption"]["termination"].update({key: value}))

    def test_reused_session_or_changed_owner_rejected(self):
        self.reject(lambda m: m["interruption"].update(resumedSession=1))
        self.reject(lambda m: m["interruption"]["resumedBinding"].update(executorBindingDigest="sha256:" + "2" * 64))

    def test_postkill_change_or_wrong_phase_rejected(self):
        self.reject(lambda m: m["interruption"]["protectedBindingAfterKill"].update(registryResourceVersion="11"))
        self.reject(lambda m: m["interruption"]["checkpoint"].update(phase="Committed"))

    def test_first_phases_cannot_claim_mutator_replay(self):
        self.reject(lambda m: m["interruption"].update(resumeAction="reserve-successor", recoveryMode="operation-replay"))

    def test_incomplete_or_unknown_driver_steps_rejected(self):
        self.reject(lambda m: m["steps"].pop(13))
        self.reject(lambda m: m["steps"][13].update(private="unallowed"))

    def test_active_old_writer_and_changed_inventory_rejected(self):
        self.reject(lambda m: m["oldWriterSafety"].update(nonterminalOldWriterPods=1))
        self.reject(lambda m: m["inventoryIdempotence"]["after"].clear())

    def test_mixed_source_and_payload_rejected(self):
        for key in ["runnerSourceCommit", "successorSubject"]:
            values = [measurement(p) for p in package.runner.PHASES]
            values[-1][key] = "different"
            with self.assertRaises(package.shared.QualificationError): package.validate_set(values)

    def test_activated_pending_resume_requires_later_completion(self):
        m = measurement("Activated")
        pending = dict(m["interruption"]["resumedResult"], outcome="upgrade-activation-pending", phase="Activated")
        m["steps"].insert(14, pending)
        m["interruption"]["resumedResult"] = pending
        m["interruption"]["resumedBinding"]["phase"] = "Activated"
        package.validate_measurement(m)
        m["steps"].pop(15)
        with self.assertRaises(package.shared.QualificationError): package.validate_measurement(m)

    def test_current_archive_requires_shared_sampling_and_collector_closure(self):
        stream=io.BytesIO();frozen={}
        with tarfile.open(fileobj=stream,mode='w') as archive:
            for alias,path in package.SOURCES.items():
                content=(package.demo.ROOT/path).read_bytes()
                member=tarfile.TarInfo(path);member.size=len(content)
                archive.addfile(member,io.BytesIO(content))
                frozen['producer/'+alias]=package.shared.digest_bytes(content)
        raw=stream.getvalue();m={'runnerSourceCommit':'source','sourceArchiveDigest':package.shared.digest_bytes(raw),'frozenInputDigests':frozen}
        with patch.object(package.subprocess,'run',return_value=types.SimpleNamespace(stdout=raw)):
            package.source_closure(raw,m,package.demo.ROOT,'source')
            for alias in ['permission-upgrade-sampling.py','permission-upgrade-collector.py']:
                self.assertIn(alias,package.SOURCES)
                broken=copy.deepcopy(m);broken['frozenInputDigests'].pop('producer/'+alias)
                with self.assertRaisesRegex(package.shared.QualificationError,'source closure changed'):
                    package.source_closure(raw,broken,package.demo.ROOT,'source')

    def test_stale_source_commit_rejected_without_git_access(self):
        with self.assertRaises(package.shared.QualificationError):
            package.source_closure(b"archive", measurement("Planned"), package.demo.ROOT, "different")


if __name__ == "__main__":
    unittest.main()
