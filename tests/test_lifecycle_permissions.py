import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("run_lifecycle.py")
SPEC = importlib.util.spec_from_file_location("run_lifecycle", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("lifecycle runner could not be imported")
LIFECYCLE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(LIFECYCLE)


def allocation_runner(root, result):
    runner = LIFECYCLE.LifecycleRunner.__new__(LIFECYCLE.LifecycleRunner)
    runner.output = root
    runner.allocations = {}
    runner.target = {"markerUid": "marker-uid"}
    runner.issuer = root / "issuer.kubeconfig"
    runner.driver_environment = lambda *_: {}
    runner.cargo = lambda *_, **__: result
    return runner


class LifecyclePermissionTests(unittest.TestCase):
    def test_issuer_gets_only_named_marker_and_namespace_discovery_authority(self):
        runner = LIFECYCLE.LifecycleRunner.__new__(LIFECYCLE.LifecycleRunner)
        runner.control = "control"
        runner.workload = "workload"
        runner.domain = "domain"
        runner.cluster = "cluster"
        roles = []
        objects = []
        runner.role = lambda namespace, name, rules, account: roles.append(
            (namespace, name, copy.deepcopy(rules), account)
        )
        runner.apply = lambda value: objects.append(copy.deepcopy(value))

        runner.permissions()

        issuer_roles = [entry for entry in roles if entry[1] == "test-issuer"]
        self.assertEqual(len(issuer_roles), 1)
        namespace, _, namespaced_rules, account = issuer_roles[0]
        self.assertEqual((namespace, account), (runner.control, "test-issuer"))
        marker_rules = [rule for rule in namespaced_rules
                        if rule["resources"] == ["configmaps"] and rule["verbs"] == ["get"]]
        self.assertEqual(len(marker_rules), 1)
        self.assertEqual(marker_rules[0]["resourceNames"], [
            "apsides-trust-domain",
            "apsides-deploy-identity-domain",
            "apsides-test-support",
        ])

        cluster_roles = {
            item["metadata"]["name"]: item["rules"]
            for item in objects if item["kind"] == "ClusterRole"
        }
        issuer_bindings = [
            item for item in objects
            if item["kind"] == "ClusterRoleBinding"
            and item["subjects"] == [{
                "kind": "ServiceAccount",
                "name": "test-issuer",
                "namespace": runner.control,
            }]
        ]
        self.assertEqual(len(issuer_bindings), 1)
        issuer_cluster_role = issuer_bindings[0]["roleRef"]["name"]
        self.assertEqual(cluster_roles[issuer_cluster_role], [{
            "apiGroups": [""],
            "resources": ["namespaces"],
            "verbs": ["get"],
            "resourceNames": ["kube-system"],
        }])
        deployer_binding = [
            item for item in objects
            if item["kind"] == "ClusterRoleBinding"
            and item["metadata"]["name"] == "apsides-disposable-cluster"
        ]
        self.assertEqual(len(deployer_binding), 1)
        self.assertEqual(deployer_binding[0]["subjects"][0]["name"], "test-deployer")
        self.assertNotEqual(issuer_cluster_role, deployer_binding[0]["roleRef"]["name"])

        effective_rules = [*namespaced_rules, *cluster_roles[issuer_cluster_role]]
        self.assertFalse(any("*" in rule["apiGroups"] for rule in effective_rules))
        self.assertFalse(any("*" in rule["resources"] for rule in effective_rules))
        self.assertFalse(any("*" in rule["verbs"] for rule in effective_rules))
        self.assertFalse(any("nodes" in rule["resources"] for rule in effective_rules))
        writes = [rule for rule in effective_rules if any(
            verb in {"create", "update", "patch", "delete"} for verb in rule["verbs"]
        )]
        self.assertEqual(writes, [{
            "apiGroups": [""],
            "resources": ["serviceaccounts/token"],
            "verbs": ["create"],
            "resourceNames": ["test-deployer"],
        }])

    def test_allocation_failure_retains_only_static_typed_classification(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            private = "PRIVATE-DRIVER-DIAGNOSTIC"
            record = {
                "kind": "permission-ceiling",
                "pending": False,
                "message": private,
                "details": {"private": private},
            }
            stdout = "APSIDES_TEST_ERROR=" + json.dumps(record) + "\n"
            result = subprocess.CompletedProcess([], 23, stdout=stdout, stderr=private)
            runner = allocation_runner(root, result)

            with self.assertRaisesRegex(RuntimeError, "failed to read canonical test allocation"):
                runner.allocation(Path("candidate"), "upgrade")

            path = root / "allocation-error.json"
            sanitized = json.loads(path.read_text())
            self.assertEqual(set(sanitized), {"exitCode", "errors", "scope"})
            self.assertEqual(sanitized["exitCode"], 23)
            self.assertEqual(sanitized["errors"], [{"kind": "permission-ceiling", "pending": False}])
            self.assertNotIn(private, path.read_text())

    def test_untrusted_allocation_records_are_ignored_or_mapped_to_other(self):
        oversized = "APSIDES_TEST_ERROR=" + json.dumps({
            "kind": "permission-ceiling",
            "pending": False,
            "message": "x" * 4096,
        })
        cases = {
            "oversized": (oversized, []),
            "malformed": ("APSIDES_TEST_ERROR={not-json", []),
            "unapproved-kind": (
                "APSIDES_TEST_ERROR=" + json.dumps({"kind": "private-kind", "pending": True}),
                [{"kind": "other", "pending": True}],
            ),
        }
        for name, (stdout, expected) in cases.items():
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                result = subprocess.CompletedProcess([], 1, stdout=stdout + "\n", stderr="private stderr")
                runner = allocation_runner(root, result)
                with self.assertRaisesRegex(RuntimeError, "failed to read canonical test allocation"):
                    runner.allocation(Path("candidate"), "upgrade")
                sanitized = json.loads((root / "allocation-error.json").read_text())
                self.assertEqual(sanitized["errors"], expected)
                self.assertNotIn("private", (root / "allocation-error.json").read_text())


if __name__ == "__main__":
    unittest.main()
