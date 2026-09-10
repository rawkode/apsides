#!/usr/bin/env python3
"""Measure one killed-and-reaped CNPG deployment phase; never emit a release receipt."""
import argparse
import ast
import json
import os
from pathlib import Path
import selectors
import shutil
import signal
import subprocess
import sys
import time

import run_cnpg_demo as demo

shared = demo.shared
PHASES = {"Planned": "reserve-successor", "Staged": "stage-successor", "Quiesced": "quiesce-upgrade",
          "Activated": "activate-upgrade", "Cleaning": "activate-upgrade", "Committed": "commit-upgrade"}
PREFIX = "APSIDES_TEST_CHECKPOINT="


def require(condition, message):
    if not condition:
        raise shared.QualificationError(message)


def static_reasons(text):
    return {node.args[1].value for node in ast.walk(ast.parse(text))
            if isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and node.func.id == "require"
            and len(node.args) == 2 and isinstance(node.args[1], ast.Constant) and isinstance(node.args[1].value, str)}


def binding(value, prepared, phase=None, checkpoint=None):
    metadata = value["metadata"]
    registry = json.loads(value["data"]["state.json"], object_pairs_hook=shared.unique_object)
    stored = registry["transitions"][prepared["transition"]]
    record, owner = stored["record"], stored["executorBinding"]
    allocation = stored["inputs"]["allocation"]
    require(allocation["generationId"] == prepared["generation"] and allocation["transitionId"] == prepared["transition"],
            "checkpoint generation allocation differs")
    require(allocation["registryUid"] == prepared["registryUid"], "checkpoint allocation registry differs")
    require(owner["executorId"] == "candidate-pair-qualification"
            and owner["domainId"] == demo.EXPECTED_SCOPE["domainId"]
            and owner["sequence"] == allocation["sequence"]
            and owner["scope"] == {"registryUid": prepared["registryUid"],
                **{key: demo.EXPECTED_SCOPE[key] for key in ["clusterBinding", "controlNamespace", "workloadNamespace"]}},
            "checkpoint owner scope differs")
    expected_active = allocation if record["phase"] == "Committed" else stored["inputs"]["previous"]["allocation"]
    require(registry["histories"][demo.EXPECTED_SCOPE["domainId"]]["activeAllocation"] == expected_active,
            "checkpoint active allocation advanced incorrectly")
    require(metadata["uid"] == prepared["registryUid"] and record["transitionId"] == prepared["transition"],
            "checkpoint registry or transition differs")
    require(owner["transitionId"] == prepared["transition"] and owner["executorId"] == record["executorId"],
            "checkpoint executor binding differs")
    require(bool(record["executorId"]) and bool(owner["serviceAccountUid"]), "checkpoint owner is absent")
    if phase is not None:
        require(record["phase"] == phase, "checkpoint protected phase differs")
    result = {"registryUid": metadata["uid"], "registryResourceVersion": metadata["resourceVersion"],
              "transition": prepared["transition"], "generation": prepared["generation"], "phase": record["phase"],
              "executorId": record["executorId"], "executorBindingDigest": shared.digest_bytes(shared.canonical(owner)),
              "registryDigest": shared.digest_bytes(shared.canonical(value))}
    if checkpoint is not None:
        require(set(checkpoint) == {"registryUid", "registryResourceVersion", "transition", "phase"}
                and all(checkpoint[key] == result[key] for key in checkpoint), "checkpoint marker differs from API readback")
    return result


def await_record(process, timeout):
    """Discard unrelated child output; return only closed checkpoint or typed driver result."""
    require(process.stdout is not None, "checkpoint child has no output pipe")
    deadline, buffer = time.monotonic() + timeout, bytearray()
    with selectors.DefaultSelector() as selector:
        selector.register(process.stdout, selectors.EVENT_READ)
        while time.monotonic() < deadline:
            for key, _ in selector.select(0.2):
                chunk = os.read(key.fileobj.fileno(), 4096)
                if chunk:
                    buffer.extend(chunk)
                    require(len(buffer) <= 256 * 1024, "checkpoint output exceeds bound")
                    while b"\n" in buffer:
                        line, _, buffer = buffer.partition(b"\n")
                        for prefix in [PREFIX, demo.PREFIX]:
                            if line.startswith(prefix.encode()):
                                return prefix, line.decode("utf-8")
            require(process.poll() is None, "checkpoint child exited without a marker")
    require(False, "checkpoint child timed out")


def kill_and_reap(process):
    require(process.poll() is None, "checkpoint child exited before interruption")
    process.kill()
    code = process.wait(timeout=10)
    require(code == -signal.SIGKILL, "checkpoint child did not terminate by SIGKILL")
    return {"pid": process.pid, "signal": "SIGKILL", "returnCode": code, "reaped": True}


class PhaseRecoveryRunner(demo.CnpgDemoRunner):
    def __init__(self, args):
        args.retain_on_success = False
        self.recovery_phase = args.phase
        self.interruption = None
        self.issuer_sessions = 0
        super().__init__(args)
        source = Path(__file__).resolve()
        self.failure_reasons |= static_reasons(source.read_text())
        for path, alias in [(source, "phase-recovery-runner.py"),
                            (demo.ROOT / "crates/platform/src/pair_phase_interruption_live.rs", "phase-recovery-adapter.rs")]:
            require(shared.source_state(path) == (self.repository, self.commit), "phase recovery sources are not frozen")
            self.sources.append(path)
            self.source_archives[path] = alias
            self.source_digests[path] = shared.digest_file(path)
            self.freeze_file("producer/" + alias, path)

    def refresh_issuer(self):
        super().refresh_issuer()
        self.issuer_sessions += 1

    def prepare(self):
        super().prepare()
        output = self.command(["cargo", "test", "--locked", "--offline", "-p", "platform", "--features", "test-support",
                               "--lib", "--no-run", "--message-format=json"], timeout=300)
        executables = []
        for line in output.splitlines():
            try:
                row = json.loads(line)
            except ValueError:
                continue
            if row.get("reason") == "compiler-artifact" and row.get("target", {}).get("name") == "platform" and row.get("executable"):
                executables.append(row["executable"])
        require(len(executables) == 1, "Cargo did not identify one checkpoint driver")
        self.checkpoint_driver = self.output / "phase-checkpoint-driver"
        shutil.copy2(executables[0], self.checkpoint_driver)
        self.freeze_file("executable/phase-checkpoint-driver", self.checkpoint_driver)
        self.require_inputs_unchanged()

    def protected(self, phase=None, checkpoint=None):
        value = json.loads(self.k("get", "configmap", "apsides-registry-" + self.workload, "-n", self.control, "-o", "json"))
        result = binding(value, self.successor_prepared, phase, checkpoint)
        if checkpoint is not None:
            state = json.loads(value["data"]["state.json"])
            stored = state["transitions"][self.successor_prepared["transition"]]
            staged = stored["stagedIdentityUids"]
            require(isinstance(staged, dict) and all(staged.values())
                    and bool(staged) == (phase != "Planned"), "checkpoint staged identities differ")
            result["stagedIdentityCount"] = len(staged)
        return result

    def step(self, action, expected, kubeconfig=None, **extra):
        if action == PHASES[self.recovery_phase] and self.interruption is None:
            self.interrupt(action, extra)
        resume_action = action
        if self.interruption is not None and "resumedResult" not in self.interruption:
            if self.recovery_phase in {"Planned", "Staged", "Quiesced"}:
                resume_action = "resume-checkpoint"
                extra = {"transition": self.successor_prepared["transition"], "resumePhase": self.recovery_phase,
                         "expectedRequestDigest": self.successor_prepared["requestDigest"],
                         **{key: self.interruption["checkpoint"][key] for key in ["registryUid", "registryResourceVersion"]}}
            self.interruption["resumeAction"] = resume_action
            self.interruption["recoveryMode"] = "protected-checkpoint-readback" if resume_action == "resume-checkpoint" else "operation-replay"
        result = super().step(resume_action, expected, kubeconfig, **extra)
        if action == "prepare-successor":
            self.successor_prepared = result
        if action == "prepare-initial":
            self.initial_prepared = result
        if self.interruption is not None and "resumedResult" not in self.interruption:
            resumed = self.protected()
            before = self.interruption["protectedBindingBefore"]
            require(all(resumed[key] == before[key] for key in ["registryUid", "transition", "generation", "executorId", "executorBindingDigest"]),
                    "resumed process changed protected logical owner")
            self.interruption.update(resumedResult=result, resumedBinding=resumed, freshIssuedSession=self.issuer_sessions > self.interruption["interruptedSession"],
                                     resumedSession=self.issuer_sessions)
        return result

    def interrupt(self, action, extra):
        self.phase = "phase-recovery-await-" + self.recovery_phase
        settings = {"action": action, "purpose": self.purpose, "initialArtifact": str(self.initial_artifact),
                    "successorArtifact": str(self.successor_artifact), "trustPolicy": str(self.trust), "target": self.target, **extra}
        step = self.output / "checkpoint-step.json"
        step.write_bytes(shared.canonical(settings))
        pause = self.output / "checkpoint-pause.json"
        pause.write_bytes(shared.canonical({"registryUid": self.successor_prepared["registryUid"],
                                           "transition": self.successor_prepared["transition"], "phase": self.recovery_phase}))
        deadline = time.monotonic() + 360
        while time.monotonic() < deadline:
            self.refresh_issuer()
            environment = dict(os.environ, APS_PERMISSION_UPGRADE_STEP=str(step), APS_PERMISSION_UPGRADE_PAUSE=str(pause),
                               APS_PERMISSION_UPGRADE_COORDINATION_DB=str(self.coordination),
                               APS_PERMISSION_UPGRADE_INVENTORY_DB=str(self.inventory), KUBECONFIG=str(self.issuer))
            process = subprocess.Popen([str(self.checkpoint_driver), "pair_phase_interruption_live::candidate_pair_transition_checkpoint",
                                        "--ignored", "--exact", "--nocapture"], cwd=demo.ROOT,
                                       env=environment, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            try:
                prefix, line = await_record(process, min(180, deadline - time.monotonic()))
                if prefix == demo.PREFIX:
                    require(action in demo.LIFECYCLE, "reservation returned without checkpoint")
                    pending = demo.result_record(line, demo.LIFECYCLE[action][1])
                    require(pending["candidates"] == self.pair["candidates"] and pending["transition"] == self.successor_prepared["transition"],
                            "pending checkpoint changed candidate or transition")
                    require(process.wait(timeout=10) > 0, "unreached checkpoint did not fail")
                    time.sleep(2)
                    continue
                marker = json.loads(line.removeprefix(PREFIX), object_pairs_hook=shared.unique_object)
                before = self.protected(self.recovery_phase, marker)
                termination = kill_and_reap(process)
                after = self.protected(self.recovery_phase, marker)
                require(before == after, "killed checkpoint changed protected state")
                self.interruption = {"interruptedAction": action, "checkpoint": marker, "protectedBindingBefore": before,
                                     "protectedBindingAfterKill": after, "termination": termination, "interruptedSession": self.issuer_sessions}
                (self.output / "interruption.json").write_bytes(shared.canonical(self.interruption))
                return
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=10)
                if process.stdout is not None:
                    process.stdout.close()
        require(False, "checkpoint remained pending")

    def measure(self):
        super().measure()
        require(self.interruption is not None and self.interruption.get("freshIssuedSession") is True, "phase interruption was not resumed")
        transition = self.successor_prepared["transition"]
        self.final_commit = self.protected("Committed")
        self.inventory_result = next(row for row in reversed(self.steps) if row["outcome"] == "successor-inventory-published")
        inventory_before, _ = demo.base.observations.capture(self.inventory)
        self.repeated_commit = self.step("commit-upgrade", "upgrade-committed", self.issuer, transition=transition)
        require(self.protected("Committed") == self.final_commit, "repeated commit changed protected registry")
        inventory_after, _ = demo.base.observations.capture(self.inventory)
        require(inventory_after == inventory_before, "repeated commit changed inventory")
        self.inventory_stability = {"before": inventory_before, "after": inventory_after, "unchanged": True}
        name = self.initial_prepared["serviceAccountName"]
        pods = json.loads(self.k("get", "pods", "-n", self.control, "-o", "json"))["items"]
        old = [pod for pod in pods if pod.get("spec", {}).get("serviceAccountName") in {name, name + "-process"}]
        require(all(pod.get("status", {}).get("phase") in {"Succeeded", "Failed"} for pod in old), "old writer Pod remains nonterminal")
        self.old_writer_safety = {"nonterminalOldWriterPods": 0, "terminalOldWriterPodUids": sorted(pod["metadata"]["uid"] for pod in old),
                                  "retirementVerifiedByProductionActivation": True}

    def publish(self):
        self.final_check()
        manifest = self.successor_manifest
        subject = {key: manifest[key] for key in ["irDigest", "compilerDigest", "binaryDigest", "image"]}
        subject.update(releaseDigest=self.pair["candidates"]["successorReleaseDigest"], sourceInputsDigest=manifest["source"]["inputsDigest"],
                       payloadFiles={name: manifest["files"][name] for name in ["build-inputs.json", "controller.oci.tar", "apsides-controller"]})
        measurement = {"schemaVersion": "apsides.dev/cnpg-phase-recovery-measurement/v1", "passed": True, "phase": self.recovery_phase,
                       "candidatePair": self.pair["candidates"], "successorSubject": subject, "runnerSourceCommit": self.commit,
                       "sourceArchiveDigest": self.snapshot["producer/source-archive"], "runnerSourceDigest": self.source_digests[Path(__file__).resolve()],
                       "frozenInputDigests": self.snapshot, "interruption": self.interruption, "finalCommit": self.final_commit,
                       "inventory": self.inventory_result, "inventoryIdempotence": self.inventory_stability, "repeatedCommit": self.repeated_commit, "oldWriterSafety": self.old_writer_safety,
                       "workloadChecks": self.checks, "steps": self.steps, "cluster": self.cluster,
                       "clusterDeleted": True, "credentialsRemoved": True, "releaseVerified": False,
                       "limitation": "one post-CAS owned process killed and reaped; same logical owner, no stalled-executor takeover"}
        files = {"phase-recovery-runner.py": Path(__file__).read_bytes(), "phase-recovery-source.tar": self.source_archive.read_bytes(),
                 "phase-recovery-operator.yaml": self.operator_bytes, "phase-recovery-cleanup.json": (self.output / "cleanup.json").read_bytes(),
                 "measurement.json": shared.canonical(measurement)}
        require(all(0 < len(raw) <= shared.MAX_EVIDENCE_BYTES for raw in files.values()), "phase measurement exceeds bound")
        self.require_inputs_unchanged()
        evidence = self.output / "evidence"
        evidence.mkdir()
        for name, raw in files.items():
            with (evidence / name).open("xb") as stream:
                stream.write(raw)
        return evidence / "measurement.json"

    def run(self):
        passed = False
        try:
            self.prepare()
            self.phase = "cluster-create"
            self.create_cluster()
            self.phase = "bootstrap"
            self.bootstrap()
            self.phase = "lifecycle"
            self.measure()
            passed = True
        except Exception as error:
            self.primary_failure = {"phase": self.phase, "reason": demo.base.failure_reason(self, error)}
            raise
        finally:
            self.phase = "cleanup"
            self.finish(passed)
        path = self.publish()
        result = {"passed": True, "phase": self.recovery_phase, "measurement": str(path), "clusterDeleted": True,
                  "credentialsRemoved": True, "releaseVerified": False, "productionEvidenceStatus": "not-produced"}
        (self.output / "result.json").write_bytes(shared.canonical(result))
        print(json.dumps(result))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["initial-artifact", "successor-artifact", "trust-policy", "operator-manifest", "output"]:
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--operator-sha256", required=True)
    parser.add_argument("--phase", choices=PHASES, required=True)
    runner = None
    try:
        runner = PhaseRecoveryRunner(parser.parse_args())
        runner.run()
    except Exception as error:
        if runner is not None:
            (runner.output / "result.json").write_bytes(shared.canonical({"passed": False, "phase": runner.phase,
                "failureReason": demo.base.failure_reason(runner, error), "primaryFailure": runner.primary_failure,
                "clusterDisposition": runner.disposition, "releaseVerified": False}))
        print("CNPG phase recovery failed; no release receipt produced.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
