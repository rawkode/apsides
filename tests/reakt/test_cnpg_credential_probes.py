import copy
import json
import ssl
import unittest

import cnpg_credential_probes as probes


class CredentialProbes(unittest.TestCase):
    def fixture(self):
        capabilities = [
            {"apiGroup": "", "resource": "secrets", "namespace": "default", "verb": verb,
             "names": ["database-app"], "subresource": None} for verb in ["get", "list", "watch"]]
        capabilities += [
            {"apiGroup": "", "resource": "configmaps", "namespace": "default", "verb": verb,
             "names": [] if verb == "create" else ["api-config"], "subresource": None}
            for verb in ["create", "patch", "update"]]
        plan = {"ir": {"domain": {"namespace": "default", "controlNamespace": "apsides-system", "id": "database-demo"}},
                "enforcementGaps": sorted(probes.GAPS), "capabilities": capabilities,
                "requestPlan": [{"capability": c} for c in capabilities]}
        objects = {("", "configmaps", "default", "api-config"): {
            "apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "api-config", "namespace": "default", "resourceVersion": "42"},
            "data": {"private": "CANARY-NEVER-REPORT"}}}
        plan["ir"]["resources"] = [{"apiVersion": "v1", "kind": "ConfigMap", "namespace": "default", "name": "api-config", "desired": {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "api-config", "namespace": "default"}, "data": {"public": "declared"}}}]
        return plan, objects

    def test_scoped_reads_and_disclosed_gaps_are_real_requests(self):
        plan, objects = self.fixture()
        spec = probes.plan_probes(plan, objects)
        self.assertEqual(spec['unsupportedGaps'], [])
        self.assertEqual(spec['outstandingObservations'], [probes.SENSITIVE])
        requests = spec['requests']
        watch = next(r for r in requests if r['expected'] == [200] and r['query'].get('watch'))
        self.assertEqual(watch['query']['fieldSelector'], 'metadata.name=database-app')
        self.assertTrue(any(r['query'].get('watch') and 'fieldSelector' not in r['query'] and r['expected'] == [403] for r in requests))
        self.assertTrue(any(r['method'] == 'POST' and r['expected'] == [201] for r in requests))
        for r in requests:
            if r['method'] != 'GET':
                self.assertEqual(r['query']['dryRun'], 'All')

    def test_missing_objects_unknown_gaps_and_versions_fail_closed(self):
        plan, _ = self.fixture()
        spec = probes.plan_probes(plan, {})
        self.assertNotIn('rbac-create-not-name-restricted', spec['unsupportedGaps'])
        self.assertIn('rbac-update-not-field-restricted', spec['unsupportedGaps'])
        plan['enforcementGaps'].append('unknown-gap')
        self.assertIn('unknown-gap', probes.plan_probes(plan, {})['unsupportedGaps'])
        plan['requestPlan'][0]['capability']['apiGroup'] = 'unknown.example'
        with self.assertRaises(ValueError):
            probes.plan_probes(plan, {})

    def test_results_never_contain_request_bodies_and_refusal_is_failure(self):
        plan, objects = self.fixture()
        original = copy.deepcopy(objects)
        spec = probes.plan_probes(plan, objects)
        identity = {'releaseDigest': 'sha256:' + 'a' * 64, 'generation': 'g1', 'serviceAccountUid': 'uid1'}
        result = probes.execute_probes(spec, lambda *args: 403, identity)
        self.assertFalse(result['httpChecksPassed'])
        self.assertNotIn('CANARY-NEVER-REPORT', json.dumps(result))
        self.assertEqual(objects, original)
        self.assertEqual(result['outstandingObservations'], [probes.SENSITIVE])
        with self.assertRaises(ValueError):
            probes.execute_probes(spec, lambda *args: True, identity)
        with self.assertRaises(ValueError):
            probes.execute_probes(spec, lambda *args: 200, identity | {'token': 'private'})

    def test_undeclared_gap_is_never_silently_accepted(self):
        plan, objects = self.fixture()
        plan["enforcementGaps"].remove("rbac-patch-not-field-restricted")
        self.assertIn("rbac-patch-not-field-restricted", probes.plan_probes(plan, objects)["undeclaredGaps"])
        plan["capabilities"] = []
        with self.assertRaises(ValueError):
            probes.plan_probes(plan, objects)

    def test_ssa_media_type_and_unsupported_operations_are_explicit(self):
        plan, objects = self.fixture()
        spec = probes.plan_probes(plan, objects)
        ssa = next(r for r in spec["requests"] if r["id"].startswith("intended-ssa"))
        broad = next(r for r in spec["requests"] if r["id"].startswith("broader-patch"))
        self.assertEqual(ssa["contentType"], "application/apply-patch+yaml")
        self.assertEqual(broad["contentType"], "application/merge-patch+json")
        self.assertEqual(ssa["query"]["dryRun"], "All")
        self.assertEqual(ssa["path"], "/api/v1/namespaces/default/configmaps/api-config")
        self.assertEqual(probes.plan_probes(plan, {})["unsupportedOperations"], [])
        plan["requestPlan"][0]["capability"]["verb"] = "delete"
        with self.assertRaises(ValueError):
            probes.plan_probes(plan, objects)
        identity = {"releaseDigest": "wrong", "generation": "g1", "serviceAccountUid": "uid1"}
        with self.assertRaises(ValueError):
            probes.execute_probes(spec, lambda *args: 200, identity)

    def test_service_create_and_ssa_use_signed_desired_without_allocations(self):
        plan, objects = self.fixture()
        desired = {"apiVersion": "v1", "kind": "Service", "metadata": {"name": "database-api", "namespace": "default"},
                   "spec": {"selector": {"app": "database-api"}, "ports": [{"port": 80, "targetPort": 3000}]}}
        plan["ir"]["resources"].append({"apiVersion": "v1", "kind": "Service", "name": "database-api", "namespace": "default", "desired": desired})
        cap = {"apiGroup": "", "resource": "services", "namespace": "default", "verb": "create", "names": [], "subresource": None}
        plan["capabilities"].append(cap)
        plan["requestPlan"].append({"capability": cap})
        live = copy.deepcopy(desired)
        live["spec"].update(clusterIP="10.1.1.1", clusterIPs=["10.1.1.1"])
        live["metadata"].update(uid="uid", resourceVersion="123")
        live["status"] = {}
        objects[("", "services", "default", "database-api")] = live
        original = copy.deepcopy(plan)
        spec = probes.plan_probes(plan, objects)
        create = next(r for r in spec["requests"] if r["method"] == "POST" and r["path"].endswith("/services"))
        self.assertEqual(create["body"]["spec"], desired["spec"])
        self.assertEqual(create["body"]["metadata"], {"name": "aps-credential-probe", "namespace": "default"})
        self.assertNotIn("status", create["body"])
        ssa = next(r for r in spec["requests"] if r["id"].startswith("intended-ssa") and r["path"].endswith("services/database-api"))
        self.assertEqual(ssa["body"], desired)
        self.assertEqual(plan, original)

    def test_unsupported_desired_and_stale_identity_readbacks_reject(self):
        plan, objects = self.fixture()
        for mutation in ["desired", "identity", "data"]:
            changed = copy.deepcopy(plan)
            desired = changed["ir"]["resources"][0]["desired"]
            if mutation == "desired":
                del changed["ir"]["resources"][0]["desired"]
            elif mutation == "identity":
                desired["metadata"]["name"] = "foreign"
            else:
                desired["data"]["public"] = {"expression": "unsupported"}
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                probes.plan_probes(changed, objects)
        del objects[("", "configmaps", "default", "api-config")]["metadata"]["resourceVersion"]
        with self.assertRaises(ValueError):
            probes.plan_probes(plan, objects)

    def test_paths_and_tls_cannot_escape_scope(self):
        plan, objects = self.fixture()
        with self.assertRaises(ValueError):
            probes.plan_probes(plan, objects, '../escape')
        for server in ['http://localhost', 'https://user:password@localhost', 'https://localhost/other']:
            with self.assertRaises(ValueError):
                probes.https_status_transport(server, 'private', ssl.create_default_context())
        with self.assertRaises(ValueError):
            probes.https_status_transport('https://localhost', 'private', ssl._create_unverified_context())


if __name__ == '__main__':
    unittest.main()
