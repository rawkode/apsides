import json
from pathlib import Path
import tempfile
import unittest

import analyze_resources as a


class ResourceTests(unittest.TestCase):
    def fixture(self, root):
        (root/'summary.json').write_text(json.dumps({'samples': 3, 'errors': [], 'namespaces': ['default', 'apsides-system', 'cnpg-system'], 'durationSeconds': 10}))
        for i in range(3):
            d = root/f'sample-{i:04d}'
            d.mkdir()
            pods, usage = [], []
            for n, (ns, name) in enumerate(a.GROUPS['native']):
                ref = {'namespace': ns, 'name': f'pod-{n}', 'uid': f'uid-{n}'}
                c = {'name': name, 'startTime': '2026-01-01T00:00:00Z', 'cpu': {'time': f'2026-01-01T00:00:{i*5:02d}Z', 'usageCoreNanoSeconds': i*1000000000}, 'memory': {'rssBytes': 100+i, 'workingSetBytes': 200+i}}
                pods.append({'podRef': ref, 'containers': [c]})
                usage.append({'namespace': ns, 'pod': ref['name'], 'podUid': ref['uid'], 'container': name, 'containerStartTime': c['startTime'], 'cpuCoreNanoSeconds': c['cpu']['usageCoreNanoSeconds'], 'rssBytes': c['memory']['rssBytes'], 'workingSetBytes': c['memory']['workingSetBytes']})
            (d/'node.summary.json').write_text(json.dumps({'pods': pods}))
            (d/'sample.json').write_text(json.dumps({'usage': usage, 'errors': [], 'elapsedSeconds': i*5}))

    def mutate(self, root, index, change):
        d=root/f'sample-{index:04d}'
        s=json.loads((d/'sample.json').read_text()); n=json.loads((d/'node.summary.json').read_text())
        change(s,n)
        (d/'sample.json').write_text(json.dumps(s)); (d/'node.summary.json').write_text(json.dumps(n))

    def run_fixture(self, change=None):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d); self.fixture(root)
            if change: change(root)
            return a.analyze(root,'native')

    def test_complete_and_memory(self):
        r=self.run_fixture(); self.assertFalse(r['completeSection26'])
        self.assertAlmostEqual(r['groups']['controllerSupport']['cpu']['sumContainerMeanCpuCores'], .6)
        self.assertEqual(r['groups']['fullPopulation']['memory']['rssBytes']['maximumSampledBytes'],612)
        self.assertEqual(len(r['sourceDigests']),7)

    def test_namespace_scope(self):
        result = self.run_fixture()
        self.assertEqual(result['namespaces'], ['default', 'apsides-system', 'cnpg-system'])
        self.assertIn('not a cluster-wide total', result['populationScope'])
        for namespaces in [[], ['default', 'default'], [''], [123], 'default', None]:
            with self.subTest(namespaces=namespaces):
                def change(root):
                    path = root/'summary.json'
                    value = json.loads(path.read_text())
                    value['namespaces'] = namespaces
                    path.write_text(json.dumps(value))
                with self.assertRaises(a.AnalysisError):
                    self.run_fixture(change)

    def test_middle_churn_invalidates_cpu(self):
        def change(s,n):
            s['usage'][-1]['podUid']='replacement';n['pods'][-1]['podRef']['uid']='replacement'
        r=self.run_fixture(lambda p:self.mutate(p,1,change))
        self.assertEqual(r['groups']['fullPopulation']['cpu']['status'],'unavailable')
        self.assertEqual(r['groups']['controllerSupport']['cpu']['status'],'measured')

    def test_reset(self):
        def change(s,n):
            s['usage'][0]['cpuCoreNanoSeconds']=0;n['pods'][0]['containers'][0]['cpu']['usageCoreNanoSeconds']=0
        r=self.run_fixture(lambda p:self.mutate(p,2,change))
        self.assertEqual(r['groups']['controllerSupport']['cpu']['reason'],'missing or reset CPU counter')

    def test_missing_raw(self):
        with self.assertRaises(a.AnalysisError):self.run_fixture(lambda p:(p/'sample-0001/node.summary.json').unlink())

    def test_unknown_group(self):
        def change(s,n):
            s['usage'][0]['container']='unknown';n['pods'][0]['containers'][0]['name']='unknown'
        with self.assertRaises(a.AnalysisError):self.run_fixture(lambda p:self.mutate(p,1,change))

    def test_ambiguous_group(self):
        def change(s,n):
            s['usage'][0]['container']='admin';n['pods'][0]['containers'][0]['name']='admin'
        with self.assertRaises(a.AnalysisError):self.run_fixture(lambda p:self.mutate(p,1,change))

    def test_nonmonotonic_wall_interval(self):
        with self.assertRaises(a.AnalysisError):self.run_fixture(lambda p:self.mutate(p,1,lambda s,n:s.update(elapsedSeconds=0)))

    def test_stale_kubelet_timestamp(self):
        def change(s,n):n['pods'][0]['containers'][0]['cpu']['time']='2026-01-01T00:00:00Z'
        r=self.run_fixture(lambda p:self.mutate(p,1,change))
        self.assertEqual(r['groups']['controllerSupport']['cpu']['status'],'unavailable')

    def test_repeated_kubelet_snapshot(self):
        def change(s,n):
            for row, pod in zip(s['usage'], n['pods']):
                row['cpuCoreNanoSeconds']=0
                pod['containers'][0]['cpu'].update(time='2026-01-01T00:00:00Z', usageCoreNanoSeconds=0)
        r=self.run_fixture(lambda p:self.mutate(p,1,change))
        self.assertEqual(r['groups']['fullPopulation']['cpu']['status'],'measured')

    def test_projection_tamper(self):
        with self.assertRaises(a.AnalysisError):
            self.run_fixture(lambda p:self.mutate(p,1,lambda s,n:s['usage'][0].update(rssBytes=999)))

    def test_missing_memory(self):
        def change(s,n):
            s['usage'][0]['rssBytes']=None;n['pods'][0]['containers'][0]['memory']['rssBytes']=None
        r=self.run_fixture(lambda p:self.mutate(p,1,change))
        self.assertEqual(r['groups']['controllerSupport']['memory']['rssBytes']['status'],'unavailable')


if __name__=='__main__':unittest.main()
