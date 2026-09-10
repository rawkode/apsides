"""Offline safety boundaries for the exact-candidate cleanup measurement runner."""
import copy
import json
import unittest
from unittest.mock import Mock, patch

import run_cnpg_cleanup as cleanup


class CleanupChecks(unittest.TestCase):
    def test_finalizer_change_is_uid_and_version_guarded_and_preserves_others(self):
        current = {"metadata": {"uid": "original", "resourceVersion": "17", "finalizers": ["operator/hold"]}}
        patch_value = cleanup.finalizer_patch(current, True)
        self.assertEqual(patch_value[:2], [
            {"op": "test", "path": "/metadata/uid", "value": "original"},
            {"op": "test", "path": "/metadata/resourceVersion", "value": "17"},
        ])
        self.assertEqual(patch_value[2]["value"], ["operator/hold", cleanup.FINALIZER])
        current["metadata"]["finalizers"].append(cleanup.FINALIZER)
        self.assertEqual(cleanup.finalizer_patch(current, False)[2]["value"], ["operator/hold"])
        with self.assertRaises(cleanup.shared.QualificationError):
            cleanup.finalizer_patch(current, True)
        del current["metadata"]["uid"]
        with self.assertRaises(cleanup.shared.QualificationError):
            cleanup.finalizer_patch(current, False)

    def test_uid_rejection_contract_is_closed_and_explicit(self):
        value = {"schemaVersion": cleanup.demo.SCHEMA, "outcome": "cleanup-uid-rejected",
                 "candidates": {key: "sha256:" + "a" * 64 for key in cleanup.demo.PAIR_DIGESTS},
                 "transition": "transition", "generation": "generation", "phase": "Cleaning",
                 "registryUid": "registry", "registryResourceVersion": "42", "requestDigest": ""}
        line = lambda: cleanup.demo.PREFIX + json.dumps(value)
        self.assertEqual(cleanup.result_record(line(), "cleanup-uid-rejected"), value)
        with self.assertRaises(cleanup.shared.QualificationError):
            cleanup.result_record(line(), "upgrade-commit-pending")
        value["phase"] = "Committed"
        with self.assertRaises(cleanup.shared.QualificationError):
            cleanup.result_record(line(), "cleanup-uid-rejected")
        value["phase"] = "Cleaning"
        value["unexpected"] = "field"
        with self.assertRaises(cleanup.shared.QualificationError):
            cleanup.result_record(line(), "cleanup-uid-rejected")

    def role_fixture(self):
        binding = {"metadata": {"namespace": "default"},
                   "subjects": [{"kind": "ServiceAccount", "name": "runtime", "namespace": "control"}],
                   "roleRef": {"kind": "Role", "apiGroup": "rbac.authorization.k8s.io", "name": "runtime"}}
        role = {"metadata": {"name": "runtime", "namespace": "default", "uid": "role-uid"},
                "rules": [{"apiGroups": ["postgresql.cnpg.io"], "resources": ["clusters"], "verbs": ["get"]}]}
        return binding, role

    def test_runtime_role_proof_rejects_wildcards_missing_binding_and_cluster_roles(self):
        binding, role = self.role_fixture()
        check = lambda: cleanup.runtime_roles_without_backups([binding], [role], "runtime", "control", "default")
        self.assertEqual(check()[0]["uid"], "role-uid")
        for resource in ["backups", "backups/status", "backups/*", "*", "*/status", "*/scale"]:
            role["rules"][0]["resources"] = [resource]
            with self.assertRaises(cleanup.shared.QualificationError):
                check()
        role["rules"][0] = {"apiGroups": ["*"], "resources": ["backups"], "verbs": ["get"]}
        with self.assertRaises(cleanup.shared.QualificationError):
            check()
        binding["roleRef"]["kind"] = "ClusterRole"
        with self.assertRaises(cleanup.shared.QualificationError):
            check()
        with self.assertRaises(cleanup.shared.QualificationError):
            cleanup.runtime_roles_without_backups([], [role], "runtime", "control", "default")

    def pair(self):
        kinds = [("Cluster", "database", "postgresql.cnpg.io/v1", "clusters"),
                 ("ConfigMap", "api-config", "v1", "configmaps"),
                 ("Deployment", "database-api", "apps/v1", "deployments"),
                 ("Service", "database-api", "v1", "services")]
        resources = [{"kind": kind, "name": name, "namespace": "default", "apiVersion": api,
                      "apiResource": {"plural": plural, "scope": "Namespaced"}, "desired": {},
                      "lifecycle": {"deletionPolicy": "orphan"}} for kind, name, api, plural in kinds]
        domain = {"policyRef": "platform/database-demo", "id": "database-demo", "clusterBinding": "local-demo",
                  "namespace": "default", "controlNamespace": "apsides-system"}
        successor = {"ir": {"domain": domain, "resources": resources}, "capabilities": [],
                     "enforcementGaps": [], "indirectRisks": []}
        initial = copy.deepcopy(successor)
        initial["ir"]["resources"].append({"kind": "Backup", "name": cleanup.TARGET["name"],
            "namespace": "default", "apiVersion": "postgresql.cnpg.io/v1",
            "apiResource": {"plural": "backups", "scope": "Namespaced"}, "desired": {},
            "lifecycle": {"deletionPolicy": "delete"}})
        return initial, successor

    def test_policy_has_exact_cleanup_get_delete_and_no_successor_backup_capability(self):
        initial, successor = self.pair()
        policy = cleanup.cleanup_policy(initial, successor)
        caps = policy["cleanupCeiling"]["capabilities"]
        self.assertEqual({cap["verb"] for cap in caps}, {"get", "delete"})
        for cap in caps:
            self.assertEqual(cap["names"], [cleanup.TARGET["name"]])
            self.assertEqual(cap["resource"], "backups")
            self.assertEqual(cap["namespace"], "default")
            self.assertIsNone(cap["subresource"])
        successor["capabilities"] = [{"apiGroup": "postgresql.cnpg.io", "resource": "backups"}]
        with self.assertRaises(cleanup.shared.QualificationError):
            cleanup.cleanup_policy(initial, successor)

    def test_cleanup_policy_preserves_initial_caps_and_rust_order_with_deduplication(self):
        initial, successor = self.pair()
        get = {"namespace": "default", "apiGroup": "postgresql.cnpg.io", "resource": "backups",
               "subresource": None, "verb": "get", "names": [cleanup.TARGET["name"]]}
        patch_cap = dict(get, verb="patch")
        initial["capabilities"] = [patch_cap, get]
        policy = cleanup.cleanup_policy(initial, successor)
        self.assertEqual(policy["runtimeCeiling"]["capabilities"], [patch_cap, get])
        self.assertEqual(policy["cleanupCeiling"]["capabilities"], [dict(get, verb="delete"), get, patch_cap])

    def test_cleanup_pair_rejects_orphan_or_retained_resource_changes(self):
        initial, successor = self.pair()
        initial["ir"]["resources"][-1]["lifecycle"]["deletionPolicy"] = "orphan"
        with self.assertRaises(cleanup.shared.QualificationError):
            cleanup.cleanup_policy(initial, successor)
        initial, successor = self.pair()
        successor["ir"]["resources"][0]["desired"] = {"changed": True}
        with self.assertRaises(cleanup.shared.QualificationError):
            cleanup.cleanup_policy(initial, successor)

    def test_failed_measurement_always_cleans_owned_cluster_and_never_publishes(self):
        runner = object.__new__(cleanup.CnpgCleanupRunner)
        runner.prepare = Mock()
        runner.create_cluster = Mock()
        runner.bootstrap = Mock()
        runner.measure = Mock(side_effect=cleanup.shared.QualificationError("injected failure"))
        runner.finish = Mock()
        runner.publish = Mock()
        with patch.object(cleanup.demo.base, "failure_reason", return_value="fixture failure"):
            with self.assertRaises(cleanup.shared.QualificationError):
                runner.run()
        runner.finish.assert_called_once_with(False)
        runner.publish.assert_not_called()

    def test_measurement_rejects_remaining_credentials_before_publication(self):
        runner = object.__new__(cleanup.CnpgCleanupRunner)
        runner.disposition = "deleted"
        runner.cleanup_confirmed = True
        runner.admin = Mock()
        runner.admin.exists.return_value = True
        runner.verify_docker = Mock()
        with self.assertRaises(cleanup.shared.QualificationError):
            runner.publish()
        runner.verify_docker.assert_not_called()


if __name__ == "__main__":
    unittest.main()
