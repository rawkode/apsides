"""Bound CRD setup to the unchanged compiler-generated fixture contract."""
import copy
from pathlib import Path
import unittest

import run_cluster as runner


class FixtureCrds(unittest.TestCase):
    def fixture(self):
        document = {"apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
                    "metadata": {"name": "clusters.postgresql.cnpg.io"},
                    "spec": {"group": "postgresql.cnpg.io", "scope": "Namespaced",
                             "names": {"kind": "Cluster", "plural": "clusters"},
                             "versions": [{"name": "v1", "served": True, "storage": True,
                                           "schema": {"openAPIV3Schema": {"type": "object"}}}]}}
        sha = runner.digest(document)
        ir = {"resources": [{"apiVersion": "postgresql.cnpg.io/v1", "kind": "Cluster",
                             "apiResource": {"scope": "Namespaced", "plural": "clusters", "schemaSha256": sha}}]}
        return ir, sha, document

    def test_exact_crd_does_not_mutate_fixture_ir(self):
        ir, sha, document = self.fixture()
        original = copy.deepcopy(ir)
        runner.validate_crd_documents(ir, [(sha, [document])])
        self.assertEqual(ir, original)

    def test_no_crd_flow_and_missing_or_extra_declaration(self):
        runner.validate_crd_documents({"resources": []}, [])
        ir, sha, document = self.fixture()
        for selected_ir, manifests in [(ir, []), ({"resources": []}, [(sha, [document])]), (ir, [(sha, [])])]:
            with self.assertRaises(RuntimeError):
                runner.validate_crd_documents(selected_ir, manifests)

    def test_wrong_source_scope_kind_plural_version_and_webhook_reject(self):
        mutations = [lambda d: d.update(kind="Deployment"),
                     lambda d: d["spec"].update(scope="Cluster"),
                     lambda d: d["spec"]["names"].update(plural="backups"),
                     lambda d: d["spec"]["versions"][0].update(served=False),
                     lambda d: d["spec"].update(conversion={"strategy": "Webhook", "webhook": {}}),
                     lambda d: d["spec"].update(conversion={"strategy": "None", "webhook": {}}),
                     lambda d: d["metadata"].update(name="wrong"),
                     lambda d: d["metadata"].update(namespace="default")]
        for mutate in mutations:
            ir, sha, document = self.fixture()
            mutate(document)
            with self.assertRaises(RuntimeError):
                runner.validate_crd_documents(ir, [(sha, [document])])
        ir, sha, document = self.fixture()
        with self.assertRaises(RuntimeError):
            runner.validate_crd_documents(ir, [("sha256:" + "0" * 64, [document])])

    def test_duplicate_definitions_and_conflicting_schema_reject(self):
        ir, sha, document = self.fixture()
        with self.assertRaises(RuntimeError):
            runner.validate_crd_documents(ir, [(sha, [document, document])])
        ir["resources"].append(copy.deepcopy(ir["resources"][0]))
        ir["resources"][1]["apiResource"]["schemaSha256"] = "sha256:" + "0" * 64
        with self.assertRaises(RuntimeError):
            runner.validate_crd_documents(ir, [(sha, [document])])

    def test_existing_yaml_parser_rejects_duplicate_keys_and_aliases(self):
        root = Path(__file__).resolve().parents[1]
        self.assertEqual(runner.parse_crd_yaml(b"kind: CustomResourceDefinition\n", root), [{"kind": "CustomResourceDefinition"}])
        for raw in [b"kind: one\nkind: two\n", b"value: &a yes\ncopy: *a\n"]:
            with self.assertRaises(RuntimeError):
                runner.parse_crd_yaml(raw, root)

    def test_installation_uses_only_explicit_driver_ssa_and_wait(self):
        ir, sha, document = self.fixture()
        instance = object.__new__(runner.ReactorRunner)
        instance.crd_snapshots = [{"file": "fixture-crd-0.yaml", "digest": sha, "documents": [document]}]
        instance.crd_installations = []
        calls = []
        class Result:
            stdout = '{"metadata":{"uid":"installed-crd"}}'
        def kubectl(*args, **kwargs):
            calls.append((args, kwargs))
            return Result()
        instance.k = kubectl
        instance.install_crds()
        self.assertEqual(calls[0][0], ("apply", "--server-side", "--field-manager=aps-fixture-crds", "-f", "-"))
        self.assertEqual(calls[0][1]["data"], document)
        self.assertEqual(calls[1][0], ("wait", "--for=condition=Established", "crd/clusters.postgresql.cnpg.io", "--timeout=120s"))
        self.assertEqual(instance.crd_installations[0]["sourceDigest"], sha)



class ReactionMutation(unittest.TestCase):
    def test_removed_reaction_is_detected_by_the_independent_revision_oracle(self):
        instance = runner.ReactorRunner.__new__(runner.ReactorRunner)
        reaction = {"id": "reaction-a"}
        case = {"reactionId": reaction["id"], "scenario": "reaction-contribution-removed"}
        instance.ir = {"reactions": [reaction]}
        instance.fixture_cases = [case]
        instance.fixture_results = {}
        instance.target_revision = lambda _: "revision-before"
        instance.expected_revision = (
            lambda _, omitted_reaction=None:
                "revision-before" if omitted_reaction is None else "revision-after-removal"
        )
        instance.bind_fixture_coverage()
        row = instance.fixture_results[(reaction["id"], case["scenario"])]
        self.assertEqual(row["status"], "passed")
        self.assertEqual(row["evidenceFiles"], ["cluster-transcript.json"])


if __name__ == "__main__":
    unittest.main()
