#!/usr/bin/env python3
import copy
import json
from pathlib import Path
import tempfile
import types
import unittest
from unittest.mock import Mock, patch

import run_cnpg_baseline as live
import test_cnpg_baseline as fixtures


def candidate():
    p = fixtures.plan()
    names = {'pg': 'database', 'config': 'api-config', 'api': 'database-api', 'settings': 'api-settings', 'pg-app': 'database-app'}
    p['ir']['domain'] = {'id': 'database-demo', 'namespace': 'default'}
    for r in p['ir']['resources']:
        r['namespace'] = r['desired']['metadata']['namespace'] = 'default'
        r['name'] = r['desired']['metadata']['name'] = names[r['name']]
    for i in p['ir']['inputs']:
        i['namespace'] = 'default'; i['name'] = names[i['name']]
    return fixtures.refreshed(p)


def runtime():
    deployment = {'metadata': {'uid':'d','generation':1}, 'spec': {'replicas':1},
                  'status': {'observedGeneration':1,'updatedReplicas':1,'readyReplicas':1,'availableReplicas':1}}
    rs = {'metadata': {'uid':'rs','ownerReferences':[{'controller':True,'kind':'Deployment','uid':'d'}]}}
    pod = {'metadata': {'uid':'pod','annotations':{'reloader.stakater.com/last-reloaded-from':'PRIVATE_HASH'},
                       'ownerReferences':[{'controller':True,'kind':'ReplicaSet','uid':'rs'}]},
           'spec': {'containers':[{'image':'image@sha256:abc'}]},
           'status': {'conditions':[{'type':'Ready','status':'True'}],
                      'containerStatuses':[{'ready':True,'restartCount':0,'imageID':'image@sha256:abc'}]}}
    return deployment, [pod], [rs]


class BaselineLiveTests(unittest.TestCase):
    def test_regeneration_rejects_tampered_manifest_profile_and_archive(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(live.preparation.subprocess, 'run', return_value=types.SimpleNamespace(returncode=0)):
            root=Path(directory); plan=root/'plan.json'; raw=live.preparation.canonical(candidate());plan.write_bytes(raw)
            source=root/'prepared'
            live.preparation.prepare(plan,live.preparation.digest(raw),source,'python@sha256:'+'a'*64)
            profile,_=live.validate_bundle(source,plan,root/'valid')
            self.assertEqual(profile['namespace'],'default')
            for index,name in enumerate(['flux.json','profile.json','artifact.tar.gz']):
                saved=(source/name).read_bytes();(source/name).write_bytes(saved+b' ')
                with self.assertRaises(live.shared.QualificationError): live.validate_bundle(source,plan,root/f'regen-{index}')
                (source/name).write_bytes(saved)

    def test_captured_inputs_cannot_be_rebound_by_later_reread(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);runner=self.runner(root)
            runner.args=types.SimpleNamespace(prepared=root/'prepared',plan=root/'plan',operator_manifest=root/'operator',flux_crd=[root/'crd'])
            runner.args.prepared.mkdir();runner.args.plan.write_bytes(b'plan')
            raw=b'ghcr.io/cloudnative-pg/cloudnative-pg:1.30.0@sha256:'+b'a'*64
            runner.args.operator_manifest.write_bytes(raw);runner.args.flux_crd[0].write_bytes(b'crd')
            profile={'sourcePlanDigest':live.shared.digest_bytes(b'plan'),'pins':{'crds':[{'sha256':live.shared.digest_bytes(b'crd').removeprefix('sha256:')}]}}
            bundle={'profile.json':b'captured'};file=runner.args.prepared/'profile.json';file.write_bytes(b'changed')
            with patch.object(live,'validate_bundle',return_value=(profile,bundle)), patch.object(live,'OPERATOR_DIGEST',live.shared.digest_bytes(raw)):
                live.BaselineRunner.prepare(runner)
            self.assertEqual(runner.input_digests[str(file)],live.shared.digest_bytes(b'captured'))
            runner.repository=live.ROOT;runner.sources=[]
            with patch.object(live.shared,'source_state',return_value=(runner.repository,runner.commit)):
                with self.assertRaisesRegex(live.shared.QualificationError,'changed'):
                    live.BaselineRunner.frozen(runner)

    def test_rejects_wrong_scope(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(live.preparation.subprocess, 'run', return_value=types.SimpleNamespace(returncode=0)):
            root=Path(directory);plan=root/'plan.json';raw=live.preparation.canonical(fixtures.plan());plan.write_bytes(raw)
            live.preparation.prepare(plan,live.preparation.digest(raw),root/'prepared','python@sha256:'+'a'*64)
            with self.assertRaisesRegex(live.shared.QualificationError,'scope'):
                live.validate_bundle(root/'prepared',plan,root/'regen')

    def test_identity_excludes_private_annotations_and_checks_ownership(self):
        d,p,r=runtime();result=live.identity(d,p,r)
        self.assertEqual(result['podUid'],'pod');self.assertNotIn('PRIVATE',json.dumps(result))
        r[0]['metadata']['ownerReferences'][0]['uid']='other'
        self.assertIsNone(live.identity(d,p,r))

    def test_identity_rejects_stale_generation_restarts_and_multiple_pods(self):
        for mutate in [lambda d,p,r:d['status'].update(observedGeneration=0),
                         lambda d,p,r:p[0]['status']['containerStatuses'][0].update(restartCount=1),
                         lambda d,p,r:p.append(copy.deepcopy(p[0]))]:
            d,p,r=runtime();mutate(d,p,r);self.assertIsNone(live.identity(d,p,r))

    def runner(self, path):
        runner=object.__new__(live.BaselineRunner)
        runner.output=path;runner.admin=path/'admin';runner.issuer=path/'issuer'
        runner.admin.write_text('PRIVATE_CREDENTIAL');runner.issuer.write_text('PRIVATE_CREDENTIAL')
        runner.commit='commit';runner.source_digests={};runner.input_digests={}
        runner.phase='prepare';runner.failures=[];runner.checks=[];runner.cleanup_confirmed=False
        runner.prepare=Mock();runner.create_cluster=Mock();runner.install=Mock();runner.frozen=Mock()
        return runner

    def test_measure_failure_always_cleans_credentials_and_redacts_errors(self):
        with tempfile.TemporaryDirectory() as directory:
            runner=self.runner(Path(directory));runner.measure=Mock(side_effect=RuntimeError('PRIVATE_SECRET_HASH'))
            def cleanup():
                runner.admin.unlink();runner.issuer.unlink();runner.cleanup_confirmed=True
            runner.cleanup=Mock(side_effect=cleanup)
            result=runner.run()
            runner.cleanup.assert_called_once();self.assertFalse(result['passed'])
            self.assertTrue(result['credentialsRemoved']);self.assertNotIn('PRIVATE',json.dumps(result))

    def test_image_failure_reports_only_index_and_operation_and_still_cleans(self):
        for operation in ['pull', 'save', 'import']:
            with self.subTest(operation=operation), tempfile.TemporaryDirectory() as directory:
                runner=self.runner(Path(directory))
                runner.operator_image='a-private-image-reference@sha256:'+'a'*64
                runner.bundle={'managed.json':json.dumps({'items':[r['desired'] for r in candidate()['ir']['resources']]}).encode(),
                               'flux.json':b'{"items":[]}'}
                def command(args, **kwargs):
                    if (operation=='pull' and args[1]=='pull') or (operation=='save' and args[1:3]==['image','save']):
                        raise RuntimeError('PRIVATE_COMMAND_OUTPUT')
                    return ''
                runner.command=Mock(side_effect=command)
                runner.import_archive=Mock(side_effect=RuntimeError('PRIVATE_COMMAND_OUTPUT') if operation=='import' else None)
                runner.install=lambda:live.BaselineRunner.install(runner)
                runner.measure=Mock()
                def cleanup():
                    runner.admin.unlink();runner.issuer.unlink();runner.cleanup_confirmed=True
                runner.cleanup=Mock(side_effect=cleanup)
                result=runner.run()
                self.assertFalse(result['passed']);runner.cleanup.assert_called_once()
                self.assertEqual(result['failures'][-1]['phase'],f'image-0-{operation}')
                self.assertNotIn('PRIVATE',json.dumps(result));self.assertNotIn('private-image',json.dumps(result))

    def test_http_failure_persists_safe_status_and_class_after_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            runner=self.runner(Path(directory))
            runner.measure=Mock(side_effect=live.demo.HttpObservationError('status',503))
            def cleanup():
                runner.admin.unlink();runner.issuer.unlink();runner.cleanup_confirmed=True
            runner.cleanup=Mock(side_effect=cleanup)
            result=runner.run()
            self.assertFalse(result['passed']);self.assertTrue(result['credentialsRemoved'])
            self.assertEqual(result['failures'][-1]['httpStatus'],503)
            self.assertEqual(result['failures'][-1]['httpFailureClass'],'status')

    def test_cleanup_failure_prevents_success_and_redacts_errors(self):
        with tempfile.TemporaryDirectory() as directory:
            runner=self.runner(Path(directory));runner.measure=Mock();runner.cleanup=Mock(side_effect=RuntimeError('PRIVATE_TOKEN'))
            result=runner.run();self.assertFalse(result['passed']);self.assertFalse(result['credentialsRemoved'])
            self.assertNotIn('PRIVATE',json.dumps(result))

    def test_inherited_cleanup_removes_credentials_even_if_cluster_verification_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            runner=self.runner(Path(directory));runner.attempted=True
            runner.verify_docker=Mock(side_effect=live.shared.QualificationError('Docker identity changed'))
            with self.assertRaises(live.shared.QualificationError):
                live.demo.base.CeilingRunner.cleanup(runner)
            self.assertFalse(runner.admin.exists());self.assertFalse(runner.issuer.exists())

    def collection_fixture(self, directory, *, completed=True, errors=False):
        runner=self.runner(Path(directory));runner.workload='default'
        runner.args=types.SimpleNamespace(performance_samples=3,performance_interval=5)
        controllers={name:{'podUid':name,'metricsPath':'/api/v1/namespaces/default/pods/'+name+':8080/proxy/metrics'} for name in ['flux','reloader']}
        runner.controller_bindings=Mock(return_value=controllers)
        output=Path(directory)/'performance-active';output.mkdir()
        if completed:
            for index in range(3):
                folder=output/f'sample-{index:04d}';folder.mkdir()
                (folder/'sample.json').write_text(json.dumps({'elapsedSeconds':index*5,'collectionSeconds':0.1,'errors':[],
                    'usage':[{'namespace':'default','podUid':name} for name in controllers]+[{'namespace':'cnpg-system','podUid':'cnpg'}]}))
            (output/'summary.json').write_text(json.dumps({'schemaVersion':'reakt.dev/performance-samples/v1',
                'samples':3,'errors':['failure'] if errors else [],'namespaces':['default','cnpg-system'],
                'rawMetricPaths':[controllers[name]['metricsPath'] for name in sorted(controllers)]}))
        process=Mock(pid=12345);process.poll.side_effect=[None,0,0];process.wait.return_value=0
        return runner,controllers,process

    def test_collection_requires_first_sample_and_post_action_sample(self):
        with tempfile.TemporaryDirectory() as directory:
            runner,controllers,process=self.collection_fixture(directory)
            clock=[0.0]
            def action():clock[0]=6;return 'changed'
            with patch.object(live.os,'killpg') as killpg, patch.object(live.sampling.subprocess,'Popen',return_value=process) as launch, patch.object(live.time,'monotonic',side_effect=lambda:clock[0]):
                self.assertEqual(runner.collection_window('active',controllers,action),'changed')
            self.assertEqual(runner.checks[-1]['actionCompletedAfterLaunchSeconds'],6)
            self.assertFalse(runner.checks[-1]['completeSection26'])
            self.assertIn('--metrics-path',launch.call_args.args[0]);self.assertTrue(launch.call_args.kwargs['start_new_session']);process.wait.assert_called()

    def test_shared_native_window_rejects_wrong_build_generation_or_counter_reset(self):
        for fault in [None,'generation','build','counter-reset']:
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as directory:
                runner,controllers,process=self.collection_fixture(directory)
                output=runner.output/'performance-active';path='/api/v1/namespaces/default/pods/runtime:8080/proxy/status'
                expected={'domain':'domain','namespace':'default','pod':'runtime','path':path,'podUid':'flux',
                          'containerId':'containerd://runtime','restartCount':0,'generation':'generation','buildDigest':'sha256:'+'a'*64}
                samples=[]
                for index in range(3):
                    file=output/f'sample-{index:04d}'/'sample.json';sample=json.loads(file.read_text())
                    row={**expected,'triggerToCycleMicrosCount':index+1,'triggerToCycleMicrosSum':(index+1)*10,
                         'triggerToCycleMicrosMax':10,'applyCacheEntries':4,'applyCacheSerializedBytes':100,'pendingCoalescedTriggers':0}
                    if fault=='generation':row['generation']='wrong'
                    if fault=='build':row['buildDigest']='sha256:'+'b'*64
                    if fault=='counter-reset' and index==1:row['triggerToCycleMicrosCount']=0
                    sample['runtime']=[row];samples.append(sample);file.write_text(json.dumps(sample))
                summary=json.loads((output/'summary.json').read_text());summary['rawMetricPaths']=[]
                summary['runtimeDiagnostics']=live.sampling.collect.runtime_report(samples,[live.sampling.collect.runtime_target('domain='+path)])
                (output/'summary.json').write_text(json.dumps(summary))
                with patch.object(live.os,'killpg'),patch.object(live.sampling.subprocess,'Popen',return_value=process):
                    if fault:
                        with self.assertRaises(live.shared.QualificationError):
                            live.sampling.collection_window(runner,'active',controllers,namespaces=['default','cnpg-system'],
                                bindings=runner.controller_bindings,runtime_status=['domain='+path],expected_runtime=[expected])
                        self.assertEqual(runner.checks,[])
                    else:
                        live.sampling.collection_window(runner,'active',controllers,namespaces=['default','cnpg-system'],
                            bindings=runner.controller_bindings,runtime_status=['domain='+path],expected_runtime=[expected])
                        self.assertEqual(runner.checks[-1]['observer']['runtimeStatusGetsPerSample'],3)

    def test_baseline_freezes_shared_sampling_source(self):
        with tempfile.TemporaryDirectory(dir=live.ROOT) as directory:
            with patch.object(live.shared,'require_ignored'),patch.object(live.shared,'source_state',return_value=(live.ROOT,'commit')):
                runner=live.BaselineRunner(types.SimpleNamespace(output=Path(directory)/'output'))
                helper=Path(live.sampling.__file__).resolve()
                self.assertIn(helper,runner.sources)
                original=live.shared.digest_file
                with patch.object(live.shared,'digest_file',side_effect=lambda p:'changed' if p==helper else original(p)):
                    with self.assertRaisesRegex(live.shared.QualificationError,'changed'):runner.frozen()

    def test_collection_early_exit_never_runs_stimulus(self):
        with tempfile.TemporaryDirectory() as directory:
            runner,controllers,process=self.collection_fixture(directory,completed=False)
            process.poll.side_effect=None;process.poll.return_value=0;action=Mock()
            with patch.object(live.os,'killpg') as killpg, patch.object(live.sampling.subprocess,'Popen',return_value=process):
                with self.assertRaisesRegex(live.shared.QualificationError,'first complete sample'):
                    runner.collection_window('active',controllers,action)
            action.assert_not_called();process.wait.assert_called()

    def test_collection_action_error_terminates_and_reaps_before_return(self):
        with tempfile.TemporaryDirectory() as directory:
            runner,controllers,process=self.collection_fixture(directory)
            process.poll.side_effect=None;process.poll.return_value=None
            action=Mock(side_effect=RuntimeError('private action failure'))
            process.wait.side_effect=[live.sampling.subprocess.TimeoutExpired('collector',10),0]
            with patch.object(live.os,'killpg') as killpg, patch.object(live.sampling.subprocess,'Popen',return_value=process):
                with self.assertRaises(RuntimeError):runner.collection_window('active',controllers,action)
            self.assertEqual(killpg.call_args_list,[unittest.mock.call(12345,live.sampling.signal.SIGTERM),unittest.mock.call(12345,live.sampling.signal.SIGKILL)])
            process.wait.assert_called_with(timeout=10)
            self.assertEqual(runner.checks,[])

    def test_collection_rejects_late_action_or_incomplete_summary(self):
        for fault in ['late','errors']:
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as directory:
                runner,controllers,process=self.collection_fixture(directory,errors=fault=='errors');clock=[0.0]
                def action():clock[0]=12 if fault=='late' else 1
                with patch.object(live.os,'killpg') as killpg, patch.object(live.sampling.subprocess,'Popen',return_value=process),patch.object(live.time,'monotonic',side_effect=lambda:clock[0]):
                    with self.assertRaises(live.shared.QualificationError):runner.collection_window('active',controllers,action)
                self.assertEqual(runner.checks,[]);process.wait.assert_called()

    def test_http_startup_retries_only_transient_failures_with_same_identity(self):
        runner=object.__new__(live.BaselineRunner);expected={'podUid':'pod','replicaSetUid':'rs'}
        runner.workload_state=Mock(return_value=expected)
        runner.http_messages=Mock(side_effect=[live.demo.HttpObservationError('status',503),
                                              live.demo.HttpObservationError('transport'), {'status':200,'rowCount':2}])
        now=[0.0]
        with patch.object(live.time,'monotonic',side_effect=lambda:now[0]), patch.object(live.time,'sleep',side_effect=lambda seconds:now.__setitem__(0,now[0]+seconds)):
            self.assertEqual(runner.wait_http(2,expected),{'status':200,'rowCount':2})
        self.assertEqual(runner.http_messages.call_count,3);self.assertEqual(now[0],4)

    def test_http_startup_persistent_failure_reaches_bounded_deadline(self):
        runner=object.__new__(live.BaselineRunner);runner.workload_state=Mock(return_value={})
        runner.http_messages=Mock(side_effect=live.demo.HttpObservationError('status',503));now=[0.0]
        with patch.object(live.time,'monotonic',side_effect=lambda:now[0]), patch.object(live.time,'sleep',side_effect=lambda seconds:now.__setitem__(0,now[0]+seconds)):
            with self.assertRaisesRegex(live.shared.QualificationError,'deadline'):runner.wait_http(2,{},timeout=3)
        self.assertEqual(now[0],3);self.assertEqual(runner.http_messages.call_count,2)

    def test_http_startup_wrong_rows_and_other_status_fail_without_retry(self):
        for error in [live.demo.HttpObservationError('unexpected-rows',200), live.demo.HttpObservationError('status',500)]:
            runner=object.__new__(live.BaselineRunner);runner.workload_state=Mock(return_value={})
            runner.http_messages=Mock(side_effect=error)
            with patch.object(live.time,'sleep') as sleep:
                with self.assertRaises(live.demo.HttpObservationError):runner.wait_http(2,{})
                sleep.assert_not_called();runner.http_messages.assert_called_once_with(2)

    def test_http_startup_rejects_changed_identity_before_or_after_success(self):
        for states in [[{'podUid':'other'}],[{'podUid':'expected'},{'podUid':'other'}]]:
            runner=object.__new__(live.BaselineRunner);runner.workload_state=Mock(side_effect=states)
            runner.http_messages=Mock(return_value={'status':200,'rowCount':2})
            with self.assertRaisesRegex(live.shared.QualificationError,'identity changed'):
                runner.wait_http(2,{'podUid':'expected'})

    def test_successful_secret_observation_cannot_skip_restoration_check(self):
        for fault in ['changed-key','changed-uid']:
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as directory:
                runner=self.runner(Path(directory));runner.workload='default'
                original={'metadata':{'uid':'s','resourceVersion':'1'},'data':{'uri':'PRIVATE_URI'}}
                current=copy.deepcopy(original);secret_reads=0;remove_calls=[]
                def kubectl(*args, **kwargs):
                    nonlocal secret_reads
                    if args[0]=='get':
                        if args[1].startswith('configmap/'):return json.dumps({'metadata':{'resourceVersion':'1'}})
                        secret_reads+=1
                        if secret_reads==3:
                            if fault=='changed-key':current['data'][live.KEY]='OTHER_VALUE'
                            else:current['metadata']['uid']='other';current['data'].pop(live.KEY)
                        return json.dumps(current)
                    last=json.loads(args[-1])[-1]
                    if last['op']=='add':current['data'][live.KEY]=last['value']
                    else:remove_calls.append(True)
                    return ''
                runner.k=Mock(side_effect=kubectl);runner.ready=Mock(return_value={});runner.wait_http=Mock(return_value={'rowCount':1})
                with self.assertRaisesRegex(live.shared.QualificationError,'restoration'):
                    runner.secret_reaction({})
                self.assertEqual(remove_calls,[])

    def test_secret_cleanup_runs_after_failed_rollout_without_exporting_data(self):
        with tempfile.TemporaryDirectory() as directory:
            runner=self.runner(Path(directory));runner.workload='default'
            original={'metadata':{'uid':'s','resourceVersion':'1'},'data':{'uri':'PRIVATE_URI'}}
            current=copy.deepcopy(original)
            def kubectl(*args, **kwargs):
                if args[0]=='get':
                    if args[1].startswith('configmap/'):return json.dumps({'metadata':{'resourceVersion':'1'}})
                    return json.dumps(current)
                operations=json.loads(args[-1]);last=operations[-1]
                if last['op']=='add':current['data'][live.KEY]=last['value']
                else:current['data'].pop(live.KEY)
                return ''
            runner.k=Mock(side_effect=kubectl);runner.ready=Mock(side_effect=RuntimeError('PRIVATE'))
            with self.assertRaises(RuntimeError):runner.secret_reaction({})
            self.assertEqual(current,original);self.assertEqual(runner.checks,[])


if __name__=='__main__':unittest.main()
