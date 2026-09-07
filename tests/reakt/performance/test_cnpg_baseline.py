#!/usr/bin/env python3
import copy
import io
import json
from pathlib import Path
import tarfile
import tempfile
import types
import unittest
from unittest.mock import patch

import cnpg_baseline as baseline


def plan():
    resources = []
    for identity, api, kind, name, body in [
        ('cluster', 'postgresql.cnpg.io/v1', 'Cluster', 'pg', {'spec': {'instances': 1, 'storage': {'size': '1Gi'}, 'imageName': 'postgres@sha256:' + '1'*64}}),
        ('config', 'v1', 'ConfigMap', 'config', {'data': {'PGRST_DB_SCHEMAS': 'public'}}),
        ('deployment', 'apps/v1', 'Deployment', 'api', {'spec': {'replicas': 1, 'selector': {'matchLabels': {'app': 'api'}}, 'template': {'metadata': {'labels': {'app': 'api'}}, 'spec': {'containers': [{'name': 'api', 'image': 'postgrest@sha256:'+'2'*64}]}}}}),
        ('service', 'v1', 'Service', 'api', {'spec': {'ports': [{'port': 80, 'targetPort': 3000}], 'selector': {'app': 'api'}}}),
    ]:
        r = dict(id=identity, apiVersion=api, kind=kind, name=name, namespace='workload', desired=dict(apiVersion=api, kind=kind, metadata=dict(name=name, namespace='workload'), **body), lifecycle={'deletionPolicy':'orphan'})
        if kind == 'Cluster': r['apiResource'] = dict(plural='clusters', scope='Namespaced')
        resources.append(r)
    inputs = [dict(id=i, apiVersion='v1', kind=k, name=n, namespace='workload', mode=mode, projection=['data'], sensitive=k=='Secret', missing='block-dependent-mutations') for i,k,n,mode in [('config','ConfigMap','config','managed'),('settings','ConfigMap','settings','observed'),('connection','Secret','pg-app','observed')]]
    inputs[0]['managedResourceId'] = 'config'
    ir = {'testOnly': False, 'domain': {'namespace': 'workload', 'id': 'database'}, 'resources': resources, 'inputs': inputs,
          'reactions': [{'id': i['id'], 'op': 'rollout', 'target': 'deployment', 'inputs': [i['id']]} for i in inputs]}
    return {'ir': ir, 'irDigest': baseline.digest(baseline.canonical(ir))}


def refreshed(p):
    p['irDigest'] = baseline.digest(baseline.canonical(p['ir']))
    return p


class BaselineTests(unittest.TestCase):
    def test_preserves_all_desired_fields_except_two_named_annotations(self):
        p=plan(); original=copy.deepcopy(p); desired,inputs=baseline.project(p)
        self.assertEqual(p,original)
        for resource,actual in zip(p['ir']['resources'],desired):
            actual=copy.deepcopy(actual)
            if resource['kind']=='Deployment':
                self.assertEqual(actual['metadata'].pop('annotations'), {'configmap.reloader.stakater.com/reload':'config,settings','secret.reloader.stakater.com/reload':'pg-app'})
            self.assertEqual(actual,resource['desired'])
        self.assertEqual([x['name'] for x in inputs],['config','settings','pg-app'])
        self.assertFalse(any('data' in x for x in inputs))

    def test_rejects_changed_ir_and_cross_namespace(self):
        p=plan();p['ir']['inputs'][0]['namespace']='other'
        with self.assertRaises(baseline.UnsupportedProfile): baseline.project(p)
        with self.assertRaises(baseline.UnsupportedProfile): baseline.project(refreshed(p))

    def test_rejects_key_projection_binary_data_and_unsupported_reaction(self):
        for mutate in [lambda p:p['ir']['inputs'][2].update(projection=['data.password']),
                       lambda p:p['ir']['resources'][1]['desired'].update(binaryData={'key':'AA=='}),
                       lambda p:p['ir']['reactions'][0].update(op='delete')]:
            p=plan();mutate(p)
            with self.assertRaises(baseline.UnsupportedProfile): baseline.project(refreshed(p))

    def test_rejects_flux_claim_on_runtime_annotation(self):
        p=plan();p['ir']['resources'][2]['desired']['spec']['template']['metadata']['annotations']={'reloader.stakater.com/last-reloaded-from':''}
        with self.assertRaises(baseline.UnsupportedProfile): baseline.project(refreshed(p))

    def test_reloader_namespace_and_named_write_scope(self):
        objects=baseline.reloader_objects('workload','reloader@sha256:'+'3'*64,'api')
        self.assertTrue(all(x['metadata']['namespace']=='workload' for x in objects))
        role=next(x for x in objects if x['kind']=='Role')
        mutations=[r for r in role['rules'] if 'patch' in r['verbs'] and r['apiGroups']==['apps']]
        self.assertEqual(mutations[0]['resourceNames'],['api'])
        container=objects[-1]['spec']['template']['spec']['containers'][0]
        self.assertIn('--namespaces=workload',container['args']);self.assertIn('--reload-strategy=annotations',container['args'])
        self.assertNotIn('env',container)

    def test_preparation_archive_profile_and_scoped_controllers(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);source=root/'plan.json';source.write_bytes(baseline.canonical(plan()));output=root/'out'
            with patch.object(baseline.subprocess,'run',return_value=types.SimpleNamespace(returncode=0)):
                profile=baseline.prepare(source,baseline.digest(source.read_bytes()),output,'python@sha256:'+'4'*64)
            self.assertFalse(profile['releaseVerified']);self.assertEqual(profile['productionEvidenceStatus'],'not-produced')
            self.assertEqual(profile['managedResources'],4);self.assertEqual(profile['reactions'],3)
            self.assertEqual(profile['reconcileIntervalSeconds'],1);self.assertEqual(profile['expectedRuntimePollIntervalMillis'],1000)
            managed=json.loads((output/'managed.json').read_bytes())['items']
            with tarfile.open(fileobj=io.BytesIO((output/'artifact.tar.gz').read_bytes()),mode='r:gz') as archive:
                self.assertEqual(json.load(archive.extractfile('resources.json'))['items'],managed)
            controllers=json.loads((output/'flux.json').read_bytes())['items']
            self.assertTrue(all(x['metadata']['namespace']=='workload' for x in controllers))
            k=next(x for x in controllers if x['kind']=='Kustomization');self.assertFalse(k['spec']['prune']);self.assertNotIn('postBuild',k['spec'])
            self.assertNotIn('Secret',[x['kind'] for x in managed+controllers])
            with self.assertRaises(baseline.UnsupportedProfile): baseline.prepare(source,baseline.digest(source.read_bytes()),output,'python@sha256:'+'4'*64)

    def test_malformed_plan_has_typed_rejection(self):
        for invalid in [{}, {'ir': None}, {'ir': {'testOnly': False}}]:
            with self.assertRaises(baseline.UnsupportedProfile): baseline.project(invalid)

    def test_rejects_interval_different_from_current_runtime(self):
        with tempfile.TemporaryDirectory() as temp:
            source=Path(temp)/'plan.json';source.write_bytes(baseline.canonical(plan()));out=Path(temp)/'out'
            with self.assertRaises(baseline.UnsupportedProfile): baseline.prepare(source,baseline.digest(source.read_bytes()),out,'python@sha256:'+'4'*64,interval=5)
            self.assertFalse(out.exists())

    def test_changed_source_pin_rejected_before_output(self):
        with tempfile.TemporaryDirectory() as temp:
            source=Path(temp)/'plan.json';source.write_bytes(baseline.canonical(plan()));out=Path(temp)/'out'
            with self.assertRaises(baseline.UnsupportedProfile): baseline.prepare(source,'sha256:'+'0'*64,out,'python@sha256:'+'4'*64)
            self.assertFalse(out.exists())


if __name__=='__main__':unittest.main()
