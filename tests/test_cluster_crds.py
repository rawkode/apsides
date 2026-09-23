"""Fixture CRD manifests must match the IR's custom resource declarations exactly."""
import copy
from pathlib import Path
import unittest

import crds


class FixtureCrds(unittest.TestCase):
    def fixture(self):
        document = {"apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
                    "metadata": {"name": "clusters.postgresql.cnpg.io"},
                    "spec": {"group": "postgresql.cnpg.io", "scope": "Namespaced",
                             "names": {"kind": "Cluster", "plural": "clusters"},
                             "versions": [{"name": "v1", "served": True, "storage": True,
                                           "schema": {"openAPIV3Schema": {"type": "object"}}}]}}
        sha = crds.digest(document)
        ir = {"resources": [{"apiVersion": "postgresql.cnpg.io/v1", "kind": "Cluster",
                             "apiResource": {"scope": "Namespaced", "plural": "clusters", "schemaSha256": sha}}]}
        return ir, sha, document

    def test_exact_crd_does_not_mutate_fixture_ir(self):
        ir, sha, document = self.fixture()
        original = copy.deepcopy(ir)
        crds.validate_crd_documents(ir, [(sha, [document])])
        self.assertEqual(ir, original)

    def test_no_crd_flow_and_missing_or_extra_declaration(self):
        crds.validate_crd_documents({"resources": []}, [])
        ir, sha, document = self.fixture()
        for selected_ir, manifests in [(ir, []), ({"resources": []}, [(sha, [document])]), (ir, [(sha, [])])]:
            with self.assertRaises(RuntimeError):
                crds.validate_crd_documents(selected_ir, manifests)

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
                crds.validate_crd_documents(ir, [(sha, [document])])
        ir, sha, document = self.fixture()
        with self.assertRaises(RuntimeError):
            crds.validate_crd_documents(ir, [("sha256:" + "0" * 64, [document])])

    def test_duplicate_definitions_and_conflicting_schema_reject(self):
        ir, sha, document = self.fixture()
        with self.assertRaises(RuntimeError):
            crds.validate_crd_documents(ir, [(sha, [document, document])])
        ir["resources"].append(copy.deepcopy(ir["resources"][0]))
        ir["resources"][1]["apiResource"]["schemaSha256"] = "sha256:" + "0" * 64
        with self.assertRaises(RuntimeError):
            crds.validate_crd_documents(ir, [(sha, [document])])

    def test_existing_yaml_parser_rejects_duplicate_keys_and_aliases(self):
        root = Path(__file__).resolve().parents[1]
        self.assertEqual(crds.parse_crd_yaml(b"kind: CustomResourceDefinition\n", root), [{"kind": "CustomResourceDefinition"}])
        for raw in [b"kind: one\nkind: two\n", b"value: &a yes\ncopy: *a\n"]:
            with self.assertRaises(RuntimeError):
                crds.parse_crd_yaml(raw, root)


if __name__ == "__main__":
    unittest.main()
