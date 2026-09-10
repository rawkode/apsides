"""Offline guards; these tests do not qualify API enforcement."""
import base64
import copy
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

import run_api_gap as runner


class ApiGapChecks(unittest.TestCase):
    def test_failure_reports_allow_only_static_messages(self):
        reasons = runner.static_failure_reasons('require(False, "safe")\nrequire(False, f"private {value}")')
        self.assertEqual(reasons, {"safe"})
        instance = Mock(failure_reasons=reasons)
        self.assertEqual(runner.demo.base.failure_reason(instance, runner.shared.QualificationError("safe")), "safe")
        self.assertEqual(runner.demo.base.failure_reason(instance, runner.shared.QualificationError("private value")), "QualificationError")

    def credential(self):
        binding = {"namespace": "control", "serviceAccountName": "generation", "serviceAccountUid": "sa-uid",
                   "proxyPodName": "proxy", "proxyPodUid": "pod-uid", "projectionLifetimeSeconds": 3600}
        claims = {"sub": "system:serviceaccount:control:generation", "exp": 2000, "aud": ["api"],
                  "kubernetes.io": {"namespace": "control", "serviceaccount": {"name": "generation", "uid": "sa-uid"},
                                    "pod": {"name": "proxy", "uid": "pod-uid"}}}
        review = {"status": {"authenticated": True, "audiences": ["api"], "user": {
            "username": claims["sub"], "uid": "sa-uid", "extra": {"authentication.kubernetes.io/pod-uid": ["pod-uid"]}}}}
        return binding, claims, review

    def token(self, claims):
        return "header." + base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=") + ".signature"

    def test_token_requires_authenticated_exact_pod_serviceaccount_and_expiry(self):
        binding, claims, review = self.credential()
        result = runner.token_identity(self.token(claims), review, binding, 1000)
        self.assertEqual(result['expiresAt'], 2000)
        self.assertNotIn('signature', json.dumps(result))
        for change in ['unauthenticated', 'uid', 'pod', 'expired', 'long-lived', 'audience', 'bound-review']:
            c, r = copy.deepcopy(claims), copy.deepcopy(review)
            if change == 'unauthenticated': r['status']['authenticated'] = False
            elif change == 'uid': r['status']['user']['uid'] = 'other'
            elif change == 'pod': c['kubernetes.io']['pod']['uid'] = 'other'
            elif change == 'expired': c['exp'] = 1100
            elif change == 'long-lived': c['exp'] = 99999
            elif change == 'audience': r['status']['audiences'] = ['other']
            else: r['status']['user']['extra'] = {}
            with self.subTest(change=change), self.assertRaises(runner.shared.QualificationError):
                runner.token_identity(self.token(c), r, binding, 1000)

    def secret(self):
        return {'metadata': {'name': 'database-app', 'namespace': 'default', 'uid': 'uid', 'resourceVersion': '42'},
                'data': {'uri': 'private-uri', 'password': 'private-password'}}

    def test_secret_patch_only_changes_synthetic_key_and_tests_uid_rv(self):
        secret = self.secret()
        original = copy.deepcopy(secret)
        patch = runner.secret_patch(secret, 'synthetic')
        self.assertEqual([p['path'] for p in patch], ['/metadata/uid', '/metadata/resourceVersion', '/data/' + runner.KEY])
        self.assertNotIn('private-uri', json.dumps(patch))
        self.assertEqual(secret, original)
        secret['data'][runner.KEY] = 'synthetic'
        removed = runner.secret_patch(secret, 'synthetic', True)
        self.assertEqual(removed[-1], {'op': 'remove', 'path': '/data/' + runner.KEY})
        for value in ['other', None]:
            secret['data'][runner.KEY] = value
            with self.assertRaises(runner.shared.QualificationError):
                runner.secret_patch(secret, 'synthetic', True)

    def test_secret_patch_refuses_existing_key_or_foreign_identity(self):
        for field, value in [('uid', ''), ('name', 'other'), ('namespace', 'other'), ('resourceVersion', '')]:
            secret = self.secret(); secret['metadata'][field] = value
            with self.assertRaises(runner.shared.QualificationError): runner.secret_patch(secret, 'synthetic')
        secret = self.secret(); secret['data'][runner.KEY] = 'existing'
        with self.assertRaises(runner.shared.QualificationError): runner.secret_patch(secret, 'synthetic')

    def test_revision_requires_same_deployment_image_and_new_pod_rs_hmac(self):
        before = {'deploymentUid': 'd', 'selectedImageManifestDigest': 'i', 'containerImageId': 'i',
                  'revision': 'hmac-sha256:key:' + 'a' * 64, 'podUid': 'p', 'replicaSetUid': 'rs'}
        after = before | {'revision': 'hmac-sha256:key:' + 'b' * 64, 'podUid': 'new', 'replicaSetUid': 'new'}
        runner.revision_changed(before, after)
        for revision in ['hmac-sha256:private-reflection', 'hmac-sha256:', 'hmac-sha256:other:' + 'b' * 64]:
            with self.assertRaises(runner.shared.QualificationError):
                runner.revision_changed(before, after | {'revision': revision})
        for key in ['deploymentUid', 'selectedImageManifestDigest', 'revision', 'podUid', 'replicaSetUid']:
            changed = after | {key: 'other' if key in ['deploymentUid', 'selectedImageManifestDigest'] else before[key]}
            with self.subTest(key=key), self.assertRaises(runner.shared.QualificationError): runner.revision_changed(before, changed)

    def test_tls_requires_ca_without_insecure_fallback(self):
        for config in [{}, {'clusters': [{'cluster': {'server': 'https://localhost'}}]},
                       {'clusters': [{'cluster': {'insecure-skip-tls-verify': True, 'certificate-authority-data': 'bad'}}]}]:
            with self.assertRaises(runner.shared.QualificationError): runner.tls_endpoint(config)

    def test_projected_token_read_stops_if_identity_changes(self):
        instance = object.__new__(runner.ApiGapRunner)
        instance.control = 'control'
        binding = {'proxyPodName': 'proxy'}
        instance.k = Mock(return_value='private-token')
        instance.public_binding = Mock(return_value={'changed': True})
        with self.assertRaises(runner.shared.QualificationError): instance.credential(binding)
        instance.k.assert_called_once_with('exec', 'proxy', '-n', 'control', '--container', 'envoy', '--',
                                          '/bin/cat', '/var/run/secrets/kubernetes.io/serviceaccount/token')

    def test_checkpoints_are_fixed_and_publication_requires_cleanup(self):
        instance = object.__new__(runner.ApiGapRunner)
        with tempfile.TemporaryDirectory() as directory:
            instance.output = Path(directory)
            instance.checkpoint('http-probes')
            self.assertEqual(json.loads((instance.output/'progress.json').read_text()), {'stage': 'http-probes'})
            with self.assertRaises(runner.shared.QualificationError): instance.checkpoint('private arbitrary value')
        instance.disposition = 'retained'
        with self.assertRaises(runner.shared.QualificationError): instance.publish()


if __name__ == '__main__':
    unittest.main()
