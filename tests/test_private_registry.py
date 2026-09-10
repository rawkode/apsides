#!/usr/bin/env python3
"""Safety contracts for the owned private-registry fixture."""
import base64
import hashlib
import json
from pathlib import Path
import unittest
import tempfile
from types import SimpleNamespace
from unittest.mock import patch

import private_registry as registry


class RegistryTests(unittest.TestCase):
    def fixture(self):
        value = registry.PrivateRegistry('/tmp/qualification-oras')
        value.password = 'synthetic-test-password'
        value.local_host = 'localhost:12345'
        value.ca_path = Path('/tmp/qualification-ca')
        value.client_config = Path('/tmp/qualification-auth')
        return value

    def test_secret_matches_node_registry_and_scope(self):
        value = self.fixture()
        secret = value.secret_object('control', True)
        data = json.loads(base64.b64decode(secret['data']['.dockerconfigjson']))
        self.assertEqual(list(data['auths']), [value.host])
        self.assertEqual(secret['metadata'], {'name': registry.SECRET_NAME, 'namespace': 'control'})
        self.assertEqual(secret['type'], 'kubernetes.io/dockerconfigjson')
        self.assertNotEqual(secret['data'], value.secret_object('control', False)['data'])

    def test_invalid_secret_scope_and_non_boolean_mode_reject(self):
        value = self.fixture()
        for namespace, valid in [('other/namespace', True), ('Upper', True), ('', True), ('control', 'false')]:
            with self.assertRaises(registry.RegistryError):
                value.secret_object(namespace, valid)

    def test_publication_binds_remote_bytes_and_node_reference(self):
        value = self.fixture()
        config = 'sha256:' + '1' * 64
        raw = json.dumps({'config': {'digest': config}}).encode()
        digest = 'sha256:' + hashlib.sha256(raw).hexdigest()
        with patch.object(registry, 'command', side_effect=[b'', raw]) as run:
            reference = value.publish('/tmp/qualification.oci.tar', 'apsides/controller', digest)
        self.assertEqual(reference, value.host + '/apsides/controller@' + digest)
        self.assertEqual(value.publishedrefs[reference], {'manifestDigest': digest, 'configDigest': config})
        args = run.call_args_list[0].args[0]
        self.assertIn('--to-registry-config', args)
        self.assertIn('--to-plain-http=false', args)
        self.assertIn('--plain-http=false', run.call_args_list[1].args[0])
        self.assertNotIn(value.password, ' '.join(str(arg) for arg in args))

    def test_publication_does_not_accept_changed_manifest(self):
        value = self.fixture()
        with patch.object(registry, 'command', side_effect=[b'', b'{}']):
            with self.assertRaisesRegex(registry.RegistryError, 'manifest digest'):
                value.publish('/tmp/qualification.oci.tar', 'apsides/controller', 'sha256:' + '0' * 64)
        self.assertEqual(value.publishedrefs, {})

    def test_cleanup_does_not_remove_preexisting_container(self):
        value = self.fixture()
        value.container_id = 'a' * 64
        value.image_id = 'sha256:' + 'b' * 64
        value.before_containers = [value.container_id]
        state = {'Name': '/' + value.name, 'Image': value.image_id,
                 'Config': {'Labels': {'apsides.test.owner': value.owner}}}
        with patch.object(registry, 'command', return_value=json.dumps([state]).encode()) as run:
            with self.assertRaisesRegex(registry.RegistryError, 'ownership mismatch'):
                value.close()
        self.assertEqual(len(run.call_args_list), 1)
        self.assertEqual(run.call_args.args[0][:2], ['docker', 'inspect'])

    def test_cleanup_requires_owner_label(self):
        value = self.fixture()
        value.container_id = 'a' * 64
        value.image_id = 'sha256:' + 'b' * 64
        value.before_containers = []
        state = {'Name': '/' + value.name, 'Image': value.image_id, 'Config': {'Labels': {}}}
        with patch.object(registry, 'command', return_value=json.dumps([state]).encode()) as run:
            with self.assertRaisesRegex(registry.RegistryError, 'ownership mismatch'):
                value.close()
        self.assertEqual(len(run.call_args_list), 1)

    def test_node_trust_uses_effective_containerd_configuration(self):
        value = self.fixture()
        value.docker_context = 'orbstack'
        value.container_id = 'b' * 64
        value.before_containers = []
        runner = SimpleNamespace(container_id='a' * 64, docker_context='orbstack')
        config = b'[plugins."io.containerd.cri.v1.images".registry]\nconfig_path = "/etc/containerd/certs.d:/etc/docker/certs.d"\n'
        with tempfile.TemporaryDirectory() as directory:
            value.ca_path = Path(directory) / 'ca.crt'
            value.ca_path.write_bytes(b'public certificate fixture')
            with patch.object(value, 'require_owned'), patch.object(registry, 'command', side_effect=[b'', config, b'', b'', b'']) as run:
                value.install_node_trust(runner)
        self.assertEqual(run.call_args_list[1].args[0][-3:], ['containerd', 'config', 'dump'])


if __name__ == '__main__':
    unittest.main()
