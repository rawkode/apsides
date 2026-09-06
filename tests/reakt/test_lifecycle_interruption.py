import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).with_name("run_lifecycle.py")
SPEC = importlib.util.spec_from_file_location("run_lifecycle", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("lifecycle runner could not be imported")
LIFECYCLE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(LIFECYCLE)


def checkpoint(phase):
    return {
        "registryUid": "11111111-1111-4111-8111-111111111111",
        "registryResourceVersion": "43",
        "transition": "transition-b",
        "phase": phase,
    }


def protected_state(phase):
    allocation = {
        "registryUid": "11111111-1111-4111-8111-111111111111",
        "sequence": "2",
        "generationId": "generation-b",
        "serviceAccountName": "reakt-generation-b",
        "transitionId": "transition-b",
        "artifactDigest": "sha256:" + "a" * 64,
        "irDigest": "sha256:" + "b" * 64,
        "inventoryMetadataDigest": "sha256:" + "c" * 64,
    }
    previous = dict(allocation, sequence="1", generationId="generation-a",
                    serviceAccountName="reakt-generation-a", transitionId="transition-a")
    stored = {
        "inputs": {"allocation": allocation, "previous": {"allocation": previous}},
        "executorBinding": {
            "scope": {
                "registryUid": allocation["registryUid"],
                "clusterBinding": "reakt-disposable-fixture",
                "controlNamespace": "control",
                "workloadNamespace": "workload",
            },
            "domainId": "lifecycle",
            "transitionId": "transition-b",
            "sequence": "2",
            "executorId": "disposable-lifecycle-test",
        },
        "record": {"phase": phase},
        "stagedIdentityUids": {} if phase == "Planned" else {"controller": "controller-uid"},
    }
    if phase in ("Quiesced", "Activated", "Cleaning", "Committed"):
        stored["upgradeDrain"] = {
            "runtimeWriters": {"pods": [{"uid": "predecessor-pod-uid"}]},
        }
    return {
        "transitions": {"transition-b": stored},
        "histories": {"lifecycle": {
            "activeAllocation": allocation if phase == "Committed" else previous,
        }},
        "auditHeads": {"lifecycle": {
            "scope": {
                "claimScope": {"registryUid": allocation["registryUid"]},
                "domainId": "lifecycle",
                "transitionId": "transition-a",
                "sequence": "1",
            },
            "digest": "sha256:" + "d" * 64,
        }} if phase == "Committed" else {},
    }


def runner(root):
    value = LIFECYCLE.LifecycleRunner.__new__(LIFECYCLE.LifecycleRunner)
    value.output = root
    value.registry_uid = "11111111-1111-4111-8111-111111111111"
    value.control = "control"
    value.workload = "workload"
    value.domain = "lifecycle"
    value.binding = "reakt-disposable-fixture"
    value.results = []
    value.interrupt_after_phase = "Staged"
    return value


class FakeProcess:
    def __init__(self, running=True, wait_error=None, wait_return=None):
        self.running = running
        self.wait_error = wait_error
        self.wait_return = -LIFECYCLE.signal.SIGKILL if wait_return is None else wait_return
        self.kills = 0
        self.waits = []

    def poll(self):
        return None if self.running else -9

    def kill(self):
        self.kills += 1
        self.running = False

    def wait(self, timeout):
        self.waits.append(timeout)
        if self.wait_error is not None:
            raise self.wait_error
        self.running = False
        return self.wait_return


class PipeProcess:
    def __init__(self):
        read, self.write = os.pipe()
        self.stdout = os.fdopen(read, "rb", buffering=0)

    def poll(self):
        return None

    def close(self):
        os.close(self.write)
        self.stdout.close()


class LifecycleInterruptionTests(unittest.TestCase):
    def test_checkpoint_parser_accepts_only_the_closed_bounded_record(self):
        value = checkpoint("Activated")
        line = "REAKT_TEST_CHECKPOINT=" + json.dumps(value) + "\n"
        self.assertEqual(LIFECYCLE.parse_checkpoint(line), value)
        self.assertIsNone(LIFECYCLE.parse_checkpoint("ordinary test output\n"))

        invalid = [
            "REAKT_TEST_CHECKPOINT={not-json}\n",
            "REAKT_TEST_CHECKPOINT=" + json.dumps(dict(value, extra="private")),
            "REAKT_TEST_CHECKPOINT=" + json.dumps(dict(value, phase="Blocked")),
            "REAKT_TEST_CHECKPOINT=" + json.dumps(dict(value, registryResourceVersion="bad\nvalue")),
            "REAKT_TEST_CHECKPOINT=" + ("x" * 8192),
        ]
        for line in invalid:
            with self.subTest(line=line[:80]), self.assertRaisesRegex(RuntimeError, "checkpoint"):
                LIFECYCLE.parse_checkpoint(line)
        self.assertEqual(
            LIFECYCLE.parse_checkpoint_error(
                'REAKT_TEST_ERROR={"pending":true,"kind":"platform-pending"}\n'
            ),
            {"pending": True, "kind": "platform-pending"},
        )
        for record in [
            {"pending": True, "kind": "other"},
            {"pending": False, "kind": "platform-pending"},
            {"pending": True, "kind": "platform-pending", "message": "private"},
        ]:
            with self.subTest(record=record), self.assertRaisesRegex(RuntimeError, "error record"):
                LIFECYCLE.parse_checkpoint_error("REAKT_TEST_ERROR=" + json.dumps(record))

    def test_each_upgrade_phase_maps_to_the_exact_existing_action(self):
        self.assertEqual(
            {phase: LIFECYCLE.LifecycleRunner.interruption_action(phase)
             for phase in LIFECYCLE.INTERRUPTION_PHASES},
            {
                "Planned": "reserve", "Staged": "stage", "Quiesced": "quiesce",
                "Activated": "activate", "Cleaning": "activate", "Committed": "commit",
            },
        )
        with self.assertRaisesRegex(RuntimeError, "unsupported"):
            LIFECYCLE.LifecycleRunner.interruption_action("Blocked")

    def test_checkpoint_reader_handles_coalesced_lines_and_never_blocks_on_partial_lines(self):
        process = PipeProcess()
        value = checkpoint("Cleaning")
        os.write(process.write, b"ordinary output\n" +
                 ("REAKT_TEST_CHECKPOINT=" + json.dumps(value) + "\ntrailing\n").encode())
        try:
            self.assertEqual(
                LIFECYCLE.LifecycleRunner.await_checkpoint(process, 0.1),
                ("checkpoint", value),
            )
        finally:
            process.close()

        process = PipeProcess()
        error = {"pending": True, "kind": "platform-pending"}
        os.write(process.write, b"ordinary output\n" +
                 ("REAKT_TEST_ERROR=" + json.dumps(error) + "\ntrailing\n").encode())
        try:
            self.assertEqual(
                LIFECYCLE.LifecycleRunner.await_checkpoint(process, 0.1),
                ("error", error),
            )
        finally:
            process.close()

        process = PipeProcess()
        os.write(process.write, b"REAKT_TEST_CHECKPOINT={\"registryUid\":")
        try:
            with self.assertRaisesRegex(RuntimeError, "timed out"):
                LIFECYCLE.LifecycleRunner.await_checkpoint(process, 0.01)
        finally:
            process.close()

    def test_checkpoint_validation_covers_all_phase_claim_binding_and_writer_boundaries(self):
        for phase in LIFECYCLE.INTERRUPTION_PHASES:
            with self.subTest(phase=phase), tempfile.TemporaryDirectory() as directory:
                value = runner(Path(directory))
                state = protected_state(phase)
                registry = {
                    "metadata": {"uid": value.registry_uid, "resourceVersion": "43"},
                    "data": {"state.json": json.dumps(state)},
                }
                replicas = 0 if phase in ("Staged", "Quiesced", "Activated") else 1

                def get(kind, name, namespace=None):
                    if kind == "configmap":
                        return registry
                    if kind == "deployment":
                        if name == "reakt-generation-b" and phase == "Planned":
                            return None
                        if name == "reakt-generation-b":
                            return {"spec": {"replicas": replicas}}
                        return {"spec": {"replicas": 1 if phase in ("Planned", "Staged") else 0}}
                    raise AssertionError((kind, name, namespace))

                def k(*arguments, **_):
                    successor = "generation-b" in arguments[-3]
                    if successor:
                        items = []
                    elif phase in ("Planned", "Staged"):
                        items = [{"metadata": {"uid": "predecessor-pod-uid"}, "status": {"phase": "Running"}}]
                    else:
                        items = [{
                            "metadata": {"uid": "predecessor-pod-uid"},
                            "status": {"phase": "Succeeded", "containerStatuses": [{
                                "state": {"terminated": {"exitCode": 0}},
                            }]},
                        }]
                    return subprocess.CompletedProcess([], 0, stdout=json.dumps({"items": items}), stderr="")

                claim_state = "active" if phase in ("Activated", "Cleaning", "Committed") else "reserved"
                value.get = get
                value.k = k
                value.claims = lambda: [{
                    "domainId": "lifecycle", "generationId": "generation-b",
                    "transitionId": "transition-b", "state": claim_state,
                    "uid": "object-uid" if phase != "Planned" else None,
                }]
                evidence = value.validate_checkpoint(
                    checkpoint(phase), {"transition": "transition-b", "generation": "generation-b"}, phase,
                )

                self.assertEqual(evidence["phase"], phase)
                self.assertEqual(evidence["executor"], "disposable-lifecycle-test")
                self.assertTrue(evidence["sameExecutorResume"])
                self.assertIn("not within-phase crash coverage", evidence["scope"])

    def test_changed_registry_version_claim_or_premature_writer_is_rejected(self):
        cases = ["resource-version", "claim", "writer"]
        for case in cases:
            with self.subTest(case=case), tempfile.TemporaryDirectory() as directory:
                value = runner(Path(directory))
                state = protected_state("Staged")
                registry = {
                    "metadata": {"uid": value.registry_uid, "resourceVersion": "43"},
                    "data": {"state.json": json.dumps(state)},
                }
                if case == "resource-version":
                    registry["metadata"]["resourceVersion"] = "44"
                value.get = lambda kind, *_: registry if kind == "configmap" else {
                    "spec": {"replicas": 1 if case == "writer" else 0},
                }
                value.writer_observation = lambda allocation: {
                    "deploymentPresent": True, "replicas": 1 if case == "writer" else 0,
                    "podUids": [], "runningPods": 0, "terminalPods": 0,
                }
                value.claims = lambda: [{
                    "generationId": "generation-b", "transitionId": "foreign" if case == "claim" else "transition-b",
                    "state": "reserved", "uid": None,
                }]

                with self.assertRaisesRegex(RuntimeError, "registry version|claims differ|writer started"):
                    value.validate_checkpoint(
                        checkpoint("Staged"), {"transition": "transition-b", "generation": "generation-b"}, "Staged",
                    )

    def test_owned_checkpoint_child_is_always_killed_and_reaped(self):
        process = FakeProcess()
        self.assertEqual(
            LIFECYCLE.LifecycleRunner.kill_and_reap_checkpoint(process),
            -LIFECYCLE.signal.SIGKILL,
        )
        self.assertEqual(process.kills, 1)
        self.assertEqual(process.waits, [10])

        finished = FakeProcess(running=False)
        with self.assertRaisesRegex(RuntimeError, "exited before owned SIGKILL"):
            LIFECYCLE.LifecycleRunner.kill_and_reap_checkpoint(finished)
        self.assertEqual(finished.kills, 0)
        self.assertEqual(finished.waits, [])

        stuck = FakeProcess(wait_error=subprocess.TimeoutExpired("driver", 10))
        with self.assertRaisesRegex(RuntimeError, "could not be reaped"):
            LIFECYCLE.LifecycleRunner.kill_and_reap_checkpoint(stuck)
        self.assertEqual(stuck.kills, 1)

    def test_checkpoint_step_kills_owned_child_when_validation_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            value = runner(Path(directory))
            value.checkpoint_driver = Path(directory) / "checkpoint-driver"
            value.issuer = Path(directory) / "issuer.kubeconfig"
            value.target = {"markerUid": "marker"}
            value.adopted_uid = None
            value.allocation = lambda *_: {"transition": "transition-b", "generation": "generation-b"}
            value.driver_environment = lambda *_: {"FIXTURE": "1"}
            value.await_checkpoint = lambda *_: ("checkpoint", checkpoint("Staged"))
            value.validate_checkpoint = mock.Mock(side_effect=RuntimeError("synthetic protected-state drift"))
            process = FakeProcess()

            with mock.patch.object(LIFECYCLE.subprocess, "Popen", return_value=process) as popen:
                with self.assertRaisesRegex(RuntimeError, "protected-state drift"):
                    value.checkpoint_step(Path("candidate"), "upgrade", "Staged")
            self.assertEqual(process.kills, 1)
            self.assertEqual(process.waits, [10])
            arguments = popen.call_args.args[0]
            self.assertEqual(arguments[0], str(value.checkpoint_driver))
            self.assertEqual(arguments.count("phase_interruption_live::candidate_transition_checkpoint"), 1)

    def test_checkpoint_step_retries_only_typed_pending_and_reaps_each_attempt(self):
        with tempfile.TemporaryDirectory() as directory:
            value = runner(Path(directory))
            value.checkpoint_driver = Path(directory) / "checkpoint-driver"
            value.issuer = Path(directory) / "issuer.kubeconfig"
            value.target = {"markerUid": "marker"}
            value.adopted_uid = None
            value.allocation = lambda *_: {"transition": "transition-b", "generation": "generation-b"}
            value.driver_environment = lambda *_: {}
            outcomes = iter([
                ("error", {"pending": True, "kind": "platform-pending"}),
                ("checkpoint", checkpoint("Staged")),
            ])
            value.await_checkpoint = lambda *_: next(outcomes)
            value.validate_checkpoint = lambda *_: {"phase": "Staged"}
            pending = FakeProcess(wait_return=101)
            parked = FakeProcess()
            processes = iter([pending, parked])

            def spawn(*_, **__):
                process = next(processes)
                if process is parked:
                    self.assertEqual(pending.waits, [10])
                    self.assertEqual(pending.kills, 0)
                return process

            with mock.patch.object(LIFECYCLE.subprocess, "Popen", side_effect=spawn) as popen, \
                    mock.patch.object(LIFECYCLE.time, "sleep"):
                value.checkpoint_step(Path("candidate"), "upgrade", "Staged")

            self.assertEqual(popen.call_count, 2)
            self.assertEqual(pending.kills, 0)
            self.assertEqual(pending.waits, [10])
            self.assertEqual(parked.kills, 1)
            self.assertEqual(parked.waits, [10])
            self.assertEqual(value.results[-1]["pendingAttempts"], 1)
            self.assertEqual(value.results[-1]["exitCode"], -LIFECYCLE.signal.SIGKILL)

    def test_nonpending_missing_record_and_deadline_do_not_retry(self):
        cases = [
            ("nonpending", ("error", {"pending": False, "kind": "other"}), 120),
            ("missing", RuntimeError("driver exited without a complete record"), 120),
            ("deadline", ("error", {"pending": True, "kind": "platform-pending"}), 0),
        ]
        for name, outcome, timeout in cases:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                value = runner(Path(directory))
                value.checkpoint_driver = Path(directory) / "checkpoint-driver"
                value.issuer = Path(directory) / "issuer.kubeconfig"
                value.target = {"markerUid": "marker"}
                value.adopted_uid = None
                value.allocation = lambda *_: {"transition": "transition-b", "generation": "generation-b"}
                value.driver_environment = lambda *_: {}
                if isinstance(outcome, Exception):
                    value.await_checkpoint = mock.Mock(side_effect=outcome)
                    process = FakeProcess()
                else:
                    value.await_checkpoint = lambda *_: outcome
                    process = FakeProcess(wait_return=101)
                with mock.patch.object(LIFECYCLE.subprocess, "Popen", return_value=process) as popen, \
                        mock.patch.object(LIFECYCLE.time, "sleep"):
                    with self.assertRaisesRegex(RuntimeError, "failed before|without a complete|exceeded deadline"):
                        value.checkpoint_step(Path("candidate"), "upgrade", "Staged", timeout=timeout)
                self.assertEqual(popen.call_count, 0 if name == "deadline" else 1)
                expected_waits = [] if name == "deadline" else [10]
                self.assertEqual(process.waits, expected_waits)

    def test_ordinary_recovery_requires_committed_registry_and_inventory(self):
        with tempfile.TemporaryDirectory() as directory:
            value = runner(Path(directory))
            allocation = {"transition": "transition-b", "generation": "generation-b"}
            state = protected_state("Committed")
            registry = {"metadata": {"resourceVersion": "43"}, "data": {"state.json": json.dumps(state)}}
            inventory = {"generation": "generation-b", "snapshotId": "sha256:" + "e" * 64}
            value.get = lambda *_: registry
            value.committed_inventory = lambda candidate, transition: inventory
            value.interrupted_executor_binding = state["transitions"]["transition-b"]["executorBinding"]
            value.interrupted_registry_resource_version = "43"

            value.verify_interrupted_completion(Path("candidate"), allocation)

            self.assertEqual(value.results[-1]["check"], "phase-interruption-ordinary-recovery")
            self.assertTrue(value.results[-1]["committedInventoryPublished"])
            self.assertIn("six isolated runs", value.results[-1]["scope"])
            inventory["generation"] = "generation-a"
            with self.assertRaisesRegex(RuntimeError, "did not durably publish"):
                value.verify_interrupted_completion(Path("candidate"), allocation)


if __name__ == "__main__":
    unittest.main()
