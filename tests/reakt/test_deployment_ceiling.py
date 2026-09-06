import copy
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock


DIRECTORY = Path(__file__).parent
sys.path.insert(0, str(DIRECTORY))
SCRIPT = DIRECTORY / "run_deployment_ceiling.py"
SPEC = importlib.util.spec_from_file_location("run_deployment_ceiling", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("deployment ceiling runner could not be imported")
CEILING = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CEILING)
DIGEST = "sha256:" + "a" * 64
DIGEST_FIELDS = {
    "releaseDigest", "irDigest", "requestDigest", "missingCapabilityDigest",
}
DOCKER_CONTEXT = {"Name": "fixture-context", "Endpoints": {"docker": {"Host": "fixture"}}}
DOCKER_DAEMON = "fixture-daemon"


def result(outcome, **changes):
    value = {
        "schemaVersion": CEILING.SCHEMA,
        "outcome": outcome,
        **{key: DIGEST if key in DIGEST_FIELDS else "value"
           for key in CEILING.FIELDS[outcome]},
    }
    if outcome == "prepared":
        value["creationTargets"] = [{
            "namespace": "workload",
            "apiGroup": "",
            "resource": "configmaps",
            "name": "candidate",
        }]
    if outcome == "planned":
        value.update(nextSequence=2, claimCount=1, phase="Planned")
    value.update(changes)
    return value


def stdout(value):
    return "APS_CEILING_RESULT=" + json.dumps(value, separators=(",", ":"))


def cleanup_runner(root):
    runner = CEILING.CeilingRunner.__new__(CEILING.CeilingRunner)
    runner.cluster = "reakt-qual-0123456789abcdef0123456789abcdef"
    runner.node_image_id = "sha256:" + "a" * 64
    runner.container_id = None
    runner.attempted = True
    runner.owned = False
    runner.cleanup_confirmed = False
    runner.docker_context = "fixture-context"
    runner.docker_binding = {
        "contextDigest": CEILING.shared.digest_bytes(CEILING.shared.canonical(DOCKER_CONTEXT)),
        "daemonDigest": CEILING.shared.digest_bytes(DOCKER_DAEMON.encode()),
    }
    runner.prior_clusters = ["retained-cluster"]
    runner.prior_containers = ["retained-container"]
    runner.admin = root / "admin.kubeconfig"
    runner.issuer = root / "issuer.kubeconfig"
    runner.admin.write_text("admin-secret")
    runner.issuer.write_text("issuer-secret")
    return runner


def docker_identity_command(arguments, changed=False):
    if arguments == ["docker", "context", "inspect", "fixture-context"]:
        return json.dumps([DOCKER_CONTEXT])
    if arguments == ["docker", "info", "--format", "{{.ID}}"]:
        return ("changed-daemon" if changed else DOCKER_DAEMON) + "\n"
    return None


def owned_container(runner):
    node = runner.cluster + "-control-plane"
    return {
        "Id": "attempt-container",
        "Name": "/" + node,
        "Image": runner.node_image_id,
        "Config": {"Labels": {
            "io.x-k8s.kind.cluster": runner.cluster,
            "io.x-k8s.kind.role": "control-plane",
        }},
    }


class ResultRecordTests(unittest.TestCase):
    def test_accepts_each_exact_outcome_contract(self):
        for outcome in CEILING.FIELDS:
            with self.subTest(outcome=outcome):
                value = result(outcome)
                self.assertEqual(CEILING.result_record(stdout(value), outcome), value)

    def test_rejects_wrong_outcome_duplicate_records_and_duplicate_keys(self):
        with self.assertRaisesRegex(CEILING.shared.QualificationError, "unexpected contract"):
            CEILING.result_record(stdout(result("verified")), "bootstrapped")
        record = stdout(result("verified"))
        with self.assertRaisesRegex(CEILING.shared.QualificationError, "one bounded result"):
            CEILING.result_record(record + "\n" + record, "verified")
        duplicate = (
            'APS_CEILING_RESULT={"schemaVersion":"' + CEILING.SCHEMA
            + '","schemaVersion":"' + CEILING.SCHEMA + '","outcome":"verified"}'
        )
        with self.assertRaisesRegex(CEILING.shared.QualificationError, "duplicate key"):
            CEILING.result_record(duplicate, "verified")

    def test_rejects_unknown_fields_and_wrong_counters(self):
        unknown = result("verified", unexpected="value")
        with self.assertRaisesRegex(CEILING.shared.QualificationError, "unexpected contract"):
            CEILING.result_record(stdout(unknown), "verified")
        for field in ["nextSequence", "claimCount"]:
            for counter in [True, "2", -1]:
                with self.subTest(field=field, counter=counter):
                    value = result("planned", **{field: counter})
                    with self.assertRaises(CEILING.shared.QualificationError):
                        CEILING.result_record(stdout(value), "planned")
        value = result("planned", nextSequence=3)
        with self.assertRaisesRegex(CEILING.shared.QualificationError, "initial Planned"):
            CEILING.result_record(stdout(value), "planned")

    def test_rejects_malformed_digest(self):
        for digest in ["value", "sha256:" + "A" * 64, "sha256:" + "a" * 63]:
            with self.subTest(digest=digest):
                value = result("verified", releaseDigest=digest)
                with self.assertRaisesRegex(
                    CEILING.shared.QualificationError, "invalid ceiling result digest"
                ):
                    CEILING.result_record(stdout(value), "verified")


class PolicyAndSnapshotTests(unittest.TestCase):
    def test_restricted_policy_removes_a_required_capability_only(self):
        capabilities = [
            {"namespace": "workload", "apiGroup": "", "resource": "services",
             "subresource": None, "verb": "create", "names": ["service"]},
            {"namespace": "workload", "apiGroup": "", "resource": "configmaps",
             "subresource": None, "verb": "get", "names": ["config"]},
        ]
        plan = {"ir": {"domain": {
            "policyRef": "platform/example",
            "id": "example",
            "clusterBinding": "production-binding",
            "namespace": "workload",
            "controlNamespace": "control",
        }}, "capabilities": capabilities,
            "enforcementGaps": ["gap-z", "gap-a"],
            "indirectRisks": ["risk-z", "risk-a"]}
        policy, missing = CEILING.restricted_policy(plan)
        full = CEILING.ceiling(plan)
        self.assertIn(missing, capabilities)
        self.assertEqual(len(policy["runtimeCeiling"]["capabilities"]), 1)
        self.assertNotIn(missing, policy["runtimeCeiling"]["capabilities"])
        for key in ["policyRef", "domainId", "clusterBinding",
                    "workloadNamespace", "controlNamespace",
                    "acceptedEnforcementGaps", "acceptedIndirectRisks"]:
            self.assertEqual(policy["runtimeCeiling"][key], full[key])
        self.assertEqual(policy["cleanupCeiling"], full)
        self.assertEqual(plan["capabilities"], capabilities)

    def test_public_snapshot_excludes_every_raw_value(self):
        raw = {
            "objects": {"object": "sha256:digest"},
            "coordination": {"table": {"rowDigests": []}},
            "inventory": {"table": {"rowDigests": []}},
            "rawObjects": {"secret": b"private-object"},
            "rawCoordination": {"table": [b"private-row"]},
            "rawInventory": {"table": [b"private-inventory"]},
        }
        public = CEILING.public_snapshot(raw)
        self.assertEqual(set(public), {"objects", "coordination", "inventory"})
        self.assertNotIn("private", json.dumps(public))

    def test_binding_metadata_requires_exact_identity_and_never_parses_secret_body(self):
        runner = CEILING.CeilingRunner.__new__(CEILING.CeilingRunner)
        runner.control = "control"
        encoded = "ceiling-binding\tcontrol\tbinding-uid\t7\tOpaque\ttrue\t"
        self.assertEqual(runner.binding_metadata(encoded, "binding-uid"), {
            "name": "ceiling-binding",
            "namespace": "control",
            "uid": "binding-uid",
            "resourceVersion": "7",
            "type": "Opaque",
            "immutable": "true",
            "deletionTimestamp": "",
        })
        for name, changed in {
            "uid": encoded.replace("binding-uid", "replacement-uid"),
            "type": encoded.replace("Opaque", "kubernetes.io/tls"),
            "immutable": encoded.replace("true", "false"),
            "deletion": encoded + "now",
        }.items():
            with self.subTest(name=name), self.assertRaisesRegex(
                CEILING.shared.QualificationError, "Secret metadata changed"
            ):
                runner.binding_metadata(changed, "binding-uid")
        self.assertNotIn("{.data", CEILING.BINDING_PATH)
        self.assertNotIn("{.stringData", CEILING.BINDING_PATH)
        with self.assertRaises(CEILING.shared.QualificationError):
            runner.binding_metadata(json.dumps({
                "metadata": {"uid": "binding-uid"}, "data": {"private": "value"},
            }), "binding-uid")


class CleanupTests(unittest.TestCase):
    def test_command_pins_docker_context_and_scrubs_ambient_endpoint(self):
        runner = CEILING.CeilingRunner.__new__(CEILING.CeilingRunner)
        runner.docker_context = "fixture-context"
        ambient = {
            "PATH": "/bin",
            "DOCKER_CONTEXT": "ambient-context",
            "DOCKER_HOST": "tcp://ambient",
            "DOCKER_TLS_VERIFY": "1",
            "DOCKER_CERT_PATH": "/ambient/certs",
        }
        completed = CEILING.subprocess.CompletedProcess([], 0, stdout="ok\n", stderr="")
        with mock.patch.object(CEILING.subprocess, "run", return_value=completed) as run:
            self.assertEqual(
                runner.command(["docker", "ps", "-aq"], data="input", environment=ambient,
                               timeout=17),
                "ok\n",
            )
            arguments, options = run.call_args
            self.assertEqual(
                arguments[0],
                ["docker", "--context", "fixture-context", "ps", "-aq"],
            )
            self.assertEqual(options["input"], "input")
            self.assertEqual(options["timeout"], 17)
            self.assertEqual(options["env"]["DOCKER_CONTEXT"], "fixture-context")
            self.assertEqual(options["env"]["KIND_EXPERIMENTAL_PROVIDER"], "docker")
            for name in ["DOCKER_HOST", "DOCKER_TLS_VERIFY", "DOCKER_CERT_PATH"]:
                self.assertNotIn(name, options["env"])
        self.assertEqual(ambient["DOCKER_CONTEXT"], "ambient-context")
        with mock.patch.object(CEILING.subprocess, "run", return_value=completed) as run:
            runner.command(
                ["kind", "create", "cluster", "--name", "candidate"],
                environment=ambient,
            )
            arguments, options = run.call_args
            self.assertEqual(
                arguments[0],
                ["kind", "create", "cluster", "--name", "candidate"],
            )
            self.assertEqual(options["env"]["DOCKER_CONTEXT"], "fixture-context")
            self.assertEqual(options["env"]["KIND_EXPERIMENTAL_PROVIDER"], "docker")
            for name in ["DOCKER_HOST", "DOCKER_TLS_VERIFY", "DOCKER_CERT_PATH"]:
                self.assertNotIn(name, options["env"])

    def test_cleanup_refuses_replaced_or_multiple_containers_before_delete(self):
        for scenario in ["replaced", "multiple"]:
            with self.subTest(scenario=scenario), tempfile.TemporaryDirectory() as directory:
                runner = cleanup_runner(Path(directory))
                container = owned_container(runner)
                if scenario == "replaced":
                    container["Image"] = "sha256:" + "b" * 64
                    inspected = [container]
                else:
                    inspected = [container, copy.deepcopy(container)]
                calls = []

                def command(arguments, **_options):
                    calls.append(arguments)
                    identity = docker_identity_command(arguments)
                    if identity is not None:
                        return identity
                    if arguments[:4] == ["docker", "ps", "-aq", "--no-trunc"]:
                        return "attempt-container\n"
                    if arguments[:2] == ["docker", "inspect"]:
                        return json.dumps(inspected)
                    raise AssertionError(arguments)

                runner.command = command
                with self.assertRaises(CEILING.shared.QualificationError):
                    runner.cleanup()
                self.assertFalse(any(call[:3] == ["kind", "delete", "cluster"] for call in calls))
                self.assertFalse(runner.admin.exists())
                self.assertFalse(runner.issuer.exists())

    def test_failed_creation_attempt_cleans_only_exact_labeled_container(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = cleanup_runner(Path(directory))
            container = owned_container(runner)
            deleted = False
            calls = []

            def command(arguments, **_options):
                nonlocal deleted
                calls.append(arguments)
                identity = docker_identity_command(arguments)
                if identity is not None:
                    return identity
                if arguments[:3] == ["kind", "delete", "cluster"]:
                    deleted = True
                    return ""
                if arguments[:2] == ["docker", "inspect"]:
                    return json.dumps([container])
                if arguments[:4] == ["docker", "ps", "-aq", "--no-trunc"]:
                    if "--filter" in arguments:
                        return "" if deleted else "attempt-container\n"
                    return "retained-container\n"
                if arguments[:3] == ["docker", "ps", "-aq"]:
                    return ""
                raise AssertionError(arguments)

            runner.command = command
            runner.cluster_names = lambda: (["retained-cluster"] if deleted else
                                             ["retained-cluster", runner.cluster])
            self.assertTrue(runner.cleanup())
            self.assertTrue(deleted)
            self.assertIn(["kind", "delete", "cluster", "--name", runner.cluster], calls)
            self.assertEqual(runner.prior_clusters, ["retained-cluster"])
            self.assertEqual(runner.prior_containers, ["retained-container"])
            self.assertFalse(runner.admin.exists())
            self.assertFalse(runner.issuer.exists())

    def test_cleanup_refuses_changed_docker_identity_before_inspection_or_delete(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = cleanup_runner(Path(directory))
            calls = []

            def command(arguments, **_options):
                calls.append(arguments)
                identity = docker_identity_command(arguments, changed=True)
                if identity is not None:
                    return identity
                raise AssertionError(arguments)

            runner.command = command
            with self.assertRaisesRegex(
                CEILING.shared.QualificationError, "Docker context or daemon changed"
            ):
                runner.cleanup()
            self.assertFalse(any(call[:2] == ["docker", "inspect"] for call in calls))
            self.assertFalse(any(call[:3] == ["kind", "delete", "cluster"] for call in calls))
            self.assertFalse(runner.admin.exists())
            self.assertFalse(runner.issuer.exists())


class FailureTests(unittest.TestCase):
    def test_denied_snapshot_mutation_aborts_measurement(self):
        runner = CEILING.CeilingRunner.__new__(CEILING.CeilingRunner)
        runner.issuer = Path("issuer")
        runner.missing = {"verb": "create"}
        runner.verified = {"releaseDigest": "release"}
        runner.policy = {}
        runner.plan = {"ir": {"domain": {}}, "capabilities": [],
                       "enforcementGaps": [], "indirectRisks": []}
        prepared = result("prepared", requestDigest="request", registryUid="registry",
                          registryResourceVersion="1", releaseDigest="release",
                          generation="generation", transition="transition")
        denied = result("ceiling-rejected", requestDigest="request", registryUid="registry",
                        registryResourceVersion="1", releaseDigest="release",
                        generation="generation", transition="transition",
                        missingCapabilityDigest=CEILING.shared.digest_bytes(
                            CEILING.shared.canonical(runner.missing)))
        responses = iter([prepared, denied])
        runner.step = lambda *_args, **_kwargs: next(responses)
        runner.assert_targets_absent = lambda _prepared: None
        before = {"protected": "before"}
        runner.stable_baseline = lambda: before
        runner.snapshot = lambda: {"protected": "changed"}
        with self.assertRaisesRegex(CEILING.shared.QualificationError, "mutated protected"):
            runner.measure()
        self.assertFalse(hasattr(runner, "measurement"))

    def test_main_failure_removes_credentials_and_writes_no_receipt(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"

            class FailedRunner:
                published = False

                def __init__(self, args):
                    self.output = args.output
                    self.output.mkdir()
                    self.cluster = "reakt-qual-failed"
                    self.owned = False
                    self.cleanup_confirmed = False
                    self.admin = self.output / "admin.kubeconfig"
                    self.issuer = self.output / "issuer.kubeconfig"
                    self.admin.write_text("admin-secret")
                    self.issuer.write_text("issuer-secret")

                def prepare(self):
                    return None

                def create_cluster(self):
                    return None

                def bootstrap(self):
                    return None

                def measure(self):
                    raise CEILING.shared.QualificationError("denied snapshot changed")

                def cleanup(self):
                    self.admin.unlink(missing_ok=True)
                    self.issuer.unlink(missing_ok=True)
                    self.cleanup_confirmed = True
                    return True

                def publish(self):
                    FailedRunner.published = True
                    raise AssertionError("failure must not publish")

            arguments = ["run_deployment_ceiling.py", "--artifact", str(Path(directory) / "candidate"),
                         "--trust-policy", str(Path(directory) / "trust.json"),
                         "--output", str(output)]
            with mock.patch.object(CEILING, "CeilingRunner", FailedRunner), \
                    mock.patch.object(sys, "argv", arguments), \
                    mock.patch("builtins.print"):
                self.assertEqual(CEILING.main(), 1)
            self.assertFalse(FailedRunner.published)
            self.assertFalse((output / "evidence").exists())
            self.assertFalse((output / "deployment-ceiling-evidence.json").exists())
            failure = json.loads((output / "result.json").read_text())
            self.assertFalse(failure["passed"])
            self.assertTrue(failure["credentialsRemoved"])

    def test_main_records_unconfirmed_cleanup_without_masking_cleanup_error(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"

            class CleanupFailedRunner:
                published = False

                def __init__(self, args):
                    self.output = args.output
                    self.output.mkdir()
                    self.cluster = "reakt-qual-failed-cleanup"
                    self.owned = False
                    self.cleanup_confirmed = False
                    self.admin = self.output / "admin.kubeconfig"
                    self.issuer = self.output / "issuer.kubeconfig"
                    self.admin.write_text("admin-secret")
                    self.issuer.write_text("issuer-secret")

                def prepare(self):
                    return None

                def create_cluster(self):
                    return None

                def bootstrap(self):
                    return None

                def measure(self):
                    return None

                def cleanup(self):
                    self.admin.unlink(missing_ok=True)
                    self.issuer.unlink(missing_ok=True)
                    raise CEILING.shared.QualificationError("Docker identity changed")

                def publish(self):
                    CleanupFailedRunner.published = True
                    raise AssertionError("failed cleanup must not publish")

            arguments = [
                "run_deployment_ceiling.py",
                "--artifact", str(Path(directory) / "candidate"),
                "--trust-policy", str(Path(directory) / "trust.json"),
                "--output", str(output),
            ]
            with mock.patch.object(CEILING, "CeilingRunner", CleanupFailedRunner), \
                    mock.patch.object(sys, "argv", arguments), \
                    mock.patch("builtins.print"):
                self.assertEqual(CEILING.main(), 1)
            self.assertFalse(CleanupFailedRunner.published)
            self.assertFalse((output / "evidence").exists())
            failure = json.loads((output / "result.json").read_text())
            self.assertFalse(failure["passed"])
            self.assertFalse(failure["cleanupConfirmed"])
            self.assertTrue(failure["credentialsRemoved"])


if __name__ == "__main__":
    unittest.main()
