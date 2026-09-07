"""Offline regressions for the live CNPG demo's polling and image assertions."""
from contextlib import nullcontext
import hashlib
import types
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

import run_cnpg_demo as demo


class DemoChecks(unittest.TestCase):
    @staticmethod
    def digest(character="a"):
        return "sha256:" + character * 64

    def test_archive_import_normalizes_tag_without_removing_registry_port(self):
        digest = self.digest()
        cases = {
            "ghcr.io/cloudnative-pg/cloudnative-pg:1.30.0": "ghcr.io/cloudnative-pg/cloudnative-pg",
            "ghcr.io/cloudnative-pg/cloudnative-pg": "ghcr.io/cloudnative-pg/cloudnative-pg",
            "localhost:5000/team/controller:demo": "localhost:5000/team/controller",
            "localhost:5000/team/controller": "localhost:5000/team/controller",
            "localhost/controller:demo": "localhost/controller",
            "team/controller:demo": "docker.io/team/controller",
            "python:3.13": "docker.io/library/python",
        }
        for source, repository in cases.items():
            with self.subTest(source=source), tempfile.TemporaryDirectory() as directory:
                archive = Path(directory) / "image.tar"; archive.write_bytes(b"archive")
                runner = object.__new__(demo.CnpgDemoRunner)
                runner.docker_context = "test"; runner.cluster = "owned"; runner.node_platform = "linux/arm64"
                runner.command = Mock(return_value="")
                result = Mock(returncode=0, stdout=b"", stderr=b"")
                with patch.object(demo.subprocess, "run", return_value=result) as run:
                    runner.import_archive(archive, source + "@" + digest)
                args = run.call_args.args[0]
                self.assertEqual(args[args.index("--base-name") + 1], repository)
                self.assertEqual(runner.command.call_args.args[0][-1], repository + "@" + digest)

    def test_archive_import_rejects_mutable_reference_before_command(self):
        runner = object.__new__(demo.CnpgDemoRunner)
        with patch.object(demo.subprocess, "run") as run:
            with self.assertRaises(demo.shared.QualificationError):
                runner.import_archive(Path("missing"), "registry.example/image:latest")
            run.assert_not_called()

    def test_http_diagnostics_are_status_only_and_preserve_acceptance(self):
        cases = [(503, b'PRIVATE_SERVER_MESSAGE', 'status'),
                 (200, b'PRIVATE_NOT_JSON', 'invalid-json'),
                 (200, b'[{"message":"PRIVATE_ROW"}]', 'unexpected-rows'),
                 (200, b'x' * (demo.MAX_HTTP_BYTES + 1), 'oversized-body')]
        for status, body, reason in cases:
            with self.subTest(reason=reason):
                runner=object.__new__(demo.CnpgDemoRunner)
                runner.forwarded_service=lambda:nullcontext(1234)
                response=Mock(status=status);response.read.return_value=body
                connection=Mock();connection.getresponse.return_value=response
                with patch.object(demo.http.client,'HTTPConnection',return_value=connection):
                    with self.assertRaises(demo.HttpObservationError) as observed:
                        runner.http_messages(2)
                self.assertEqual(observed.exception.reason,reason)
                self.assertEqual(observed.exception.status,status)
                self.assertNotIn('PRIVATE',str(observed.exception))
                connection.close.assert_called_once()
                response.read.assert_called_once_with(demo.MAX_HTTP_BYTES+1)
        runner=object.__new__(demo.CnpgDemoRunner);runner.forwarded_service=lambda:nullcontext(1234)
        response=Mock(status=200);response.read.return_value=json.dumps([
            {'id':1,'message':'Hello from CloudNativePG'}, {'id':2,'message':'Configured by Apsides'}]).encode()
        connection=Mock();connection.getresponse.return_value=response
        with patch.object(demo.http.client,'HTTPConnection',return_value=connection):
            self.assertEqual(runner.http_messages(2)['rowCount'],2)

    def test_http_transport_failure_does_not_reflect_exception_text(self):
        runner=object.__new__(demo.CnpgDemoRunner);runner.forwarded_service=lambda:nullcontext(1234)
        connection=Mock();connection.request.side_effect=OSError('PRIVATE_TRANSPORT')
        with patch.object(demo.http.client,'HTTPConnection',return_value=connection):
            with self.assertRaises(demo.HttpObservationError) as observed:runner.http_messages(2)
        self.assertEqual(observed.exception.reason,'transport');self.assertIsNone(observed.exception.status)
        self.assertNotIn('PRIVATE',str(observed.exception));connection.close.assert_called_once()

    def native_performance_fixture(self):
        runner=object.__new__(demo.CnpgDemoRunner);runner.control='control';runner.workload='default';runner.domain='domain'
        runner.performance_successor={'transition':'t','registryUid':'registry','generation':'g','serviceAccountName':'runtime'}
        runner.pair={'candidates':{'successorReleaseDigest':self.digest()}}
        runner.successor_manifest={'image':{'reference':'controller@'+self.digest()}}
        runner.profile={'envoyImage':'envoy@'+self.digest(),'adminImage':'admin@'+self.digest()}
        allocation={'generationId':'g','transitionId':'t','artifactDigest':self.digest()}
        binding={'domainId':'domain','generationId':'g','buildDigest':self.digest(),'serviceAccountName':'runtime'}
        proxy={'deploymentName':'proxy','deploymentUid':'proxy-d','podName':'proxy-p','podUid':'proxy-p-uid',
               'envoyContainerId':'containerd://envoy','adminContainerId':'containerd://admin'}
        staged={'apps/v1/Deployment/control/runtime':'runtime-d',
                'v1/ServiceAccount/control/runtime':'sa','v1/ServiceAccount/control/runtime-process':'process-sa'}
        stored={'inputs':{'allocation':allocation},'runtimeBinding':binding,'proxyIdentity':proxy,
                'record':{'phase':'Committed'},'stagedIdentityUids':staged}
        registry={'transitions':{'t':stored},'histories':{'domain':{'activeAllocation':allocation}}}
        objects={'deployments':[],'replicasets':[],'pods':[],
                 'serviceaccounts':[{'metadata':{'name':'runtime','uid':'sa'}},{'metadata':{'name':'runtime-process','uid':'process-sa'}}]}
        for name,account,images in [('runtime','runtime-process',{'controller':runner.successor_manifest['image']['reference']}),
                                    ('proxy','runtime',{'envoy':runner.profile['envoyImage'],'admin':runner.profile['adminImage']})]:
            objects['deployments'].append({'metadata':{'name':name,'uid':name+'-d'},'spec':{'replicas':1},'status':{'readyReplicas':1}})
            objects['replicasets'].append({'metadata':{'uid':name+'-rs','ownerReferences':[{'controller':True,'kind':'Deployment','uid':name+'-d'}]}})
            objects['pods'].append({'metadata':{'name':name+'-p','uid':name+'-p-uid','ownerReferences':[{'controller':True,'kind':'ReplicaSet','uid':name+'-rs'}]},
                'spec':{'serviceAccountName':account,'containers':[{'name':k,'image':v} for k,v in images.items()]},
                'status':{'containerStatuses':[{'name':k,'ready':True,'restartCount':0,'state':{'running':{'startedAt':'now'}},'containerID':'containerd://'+k} for k in images]}})
        def kubectl(*args):
            if args[1]=='configmap':
                state=registry if args[2].startswith('reakt-registry') else {'status':'active','binding':binding}
                return json.dumps({'metadata':{'uid':'registry'},'data':{'state.json':json.dumps(state)}})
            return json.dumps({'items':objects[args[1]]})
        runner.k=Mock(side_effect=kubectl)
        return runner,stored,objects

    def test_native_performance_binds_all_containers_to_protected_candidate(self):
        runner,_,_=self.native_performance_fixture();bindings=runner.performance_bindings()
        self.assertEqual(bindings['runtime']['buildDigest'],self.digest())
        self.assertEqual(bindings['runtime']['serviceAccountUid'],'process-sa')
        self.assertEqual(set(bindings['proxy']['containers']),{'envoy','admin'})
        self.assertEqual(runner.k.call_count,6)

    def test_initial_performance_readiness_wait_is_bounded_and_rechecks_identity(self):
        for selected in [0,1]:
            with self.subTest(deployment=selected):
                runner,_,objects=self.native_performance_fixture();objects['deployments'][selected]['status']['readyReplicas']=0
                get=runner.k.side_effect
                def kubectl(*args):
                    if args[0]=='rollout':
                        self.assertEqual(args[-1],'--timeout=120s')
                        objects['deployments'][selected]['status']['readyReplicas']=1
                        return ''
                    return get(*args)
                runner.k=Mock(side_effect=kubectl)
                result=runner.performance_bindings(wait_ready=True)
                self.assertEqual(set(result),{'runtime','proxy'})
                self.assertEqual(sum(call.args[0]=='rollout' for call in runner.k.call_args_list),1)

    def test_initial_performance_wait_refuses_replaced_deployment(self):
        runner,_,objects=self.native_performance_fixture();objects['deployments'][0]['status']['readyReplicas']=0
        get=runner.k.side_effect
        def kubectl(*args):
            if args[0]=='rollout':
                objects['deployments'][0]['metadata']['uid']='replacement'
                objects['deployments'][0]['status']['readyReplicas']=1
                return ''
            return get(*args)
        runner.k=Mock(side_effect=kubectl)
        with self.assertRaisesRegex(demo.shared.QualificationError,'Deployment identity changed'):
            runner.performance_bindings(wait_ready=True)

    def test_window_bindings_never_wait_for_readiness_or_replaced_identity(self):
        for replaced in [False,True]:
            runner,_,objects=self.native_performance_fixture();objects['deployments'][0]['status']['readyReplicas']=0
            if replaced:objects['deployments'][0]['metadata']['uid']='replacement'
            reason='Deployment identity changed' if replaced else 'Deployment is not ready'
            with self.assertRaisesRegex(demo.shared.QualificationError,reason):runner.performance_bindings(wait_ready=replaced)
            self.assertFalse(any(call.args[0]=='rollout' for call in runner.k.call_args_list))
        self.assertIn('native performance Deployment is not ready',demo.PERFORMANCE_FAILURE_REASONS)

    def test_native_performance_rejects_wrong_allocation_account_image_or_container(self):
        for mutate in [lambda s,o:s['inputs']['allocation'].update(artifactDigest=self.digest('b')),
                       lambda s,o:s['inputs']['allocation'].update(generationId='wrong'),
                       lambda s,o:s['proxyIdentity'].update(adminContainerId='replaced'),
                       lambda s,o:o['serviceaccounts'][1]['metadata'].update(uid='replaced'),
                       lambda s,o:o['pods'][0]['spec']['containers'][0].update(image='other'),
                       lambda s,o:o['pods'][1]['status']['containerStatuses'][1].update(restartCount=1)]:
            runner,stored,objects=self.native_performance_fixture();mutate(stored,objects)
            with self.assertRaises(demo.shared.QualificationError):runner.performance_bindings()

    def test_native_predicate_diagnostics_expose_only_static_failure(self):
        runner,stored,objects=self.native_performance_fixture()
        runner.failure_reasons=demo.PERFORMANCE_FAILURE_REASONS | demo.sampling.SAFE_FAILURE_REASONS
        stored['inputs']['allocation']['artifactDigest']='PRIVATE_UNKNOWN_DIGEST'
        with self.assertRaises(demo.shared.QualificationError) as observed:runner.performance_bindings()
        self.assertEqual(demo.base.failure_reason(runner,observed.exception),'native performance allocation differs from signed release')
        self.assertNotIn('PRIVATE',demo.base.failure_reason(runner,observed.exception))

    def test_sampling_sources_are_frozen_and_changed_helper_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            for name in ['initial','successor']:
                (root/name).mkdir();(root/name/'release.json').write_text('{}')
            operator=root/'operator.yaml';image='ghcr.io/cloudnative-pg/cloudnative-pg:1.30.0@'+self.digest()
            operator.write_text('image: '+image+'\nvalue: '+image+'\n');trust=root/'trust';trust.write_text('{}')
            args=types.SimpleNamespace(initial_artifact=root/'initial',successor_artifact=root/'successor',trust_policy=trust,
                                      operator_manifest=operator,operator_sha256=demo.shared.digest_file(operator),
                                      output=root/'out',retain_on_success=False)
            def base_init(self,args):
                self.output=args.output;self.output.mkdir();self.repository=demo.ROOT;self.commit='source'
                self.artifact=args.artifact;self.trust=args.trust_policy;self.failure_reasons=set()
            def archive(command,**kwargs):
                Path(command[command.index('--output')+1]).write_bytes(b'source archive')
                return types.SimpleNamespace(returncode=0)
            with patch.object(demo.base.CeilingRunner,'__init__',base_init),patch.object(demo,'release_files',return_value={}),                 patch.object(demo,'reject_ignored_driver_sources'),patch.object(demo.subprocess,'run',side_effect=archive):
                runner=demo.CnpgDemoRunner(args)
            for reason in ["native performance allocation differs from signed release", "workload identity changed during HTTP startup"]:
                self.assertEqual(demo.base.failure_reason(runner,demo.shared.QualificationError(reason)),reason)
            self.assertEqual(demo.base.failure_reason(runner,demo.shared.QualificationError('PRIVATE_SECRET=unexpected')),'QualificationError')
            helper=Path(demo.sampling.__file__).resolve();collector=helper.parent/'collect.py'
            for path in [helper,collector]:
                self.assertIn(path,runner.sources);self.assertIn(path,runner.source_archives)
                self.assertIn('producer/'+runner.source_archives[path],runner.snapshot)
            original=demo.shared.digest_file
            with patch.object(demo,'reject_ignored_driver_sources'),patch.object(demo.shared,'source_state',return_value=(runner.repository,runner.commit)),                 patch.object(demo.shared,'digest_file',side_effect=lambda p:self.digest('b') if p==helper else original(p)):
                with self.assertRaisesRegex(demo.shared.QualificationError,'inputs changed'):runner.require_inputs_unchanged()

    def candidates(self):
        return {name: self.digest() for name in demo.PAIR_DIGESTS}

    def prepared(self, creation, managed):
        return {
            "schemaVersion": demo.SCHEMA,
            "outcome": "rejection-prepared",
            "candidates": self.candidates(),
            "generation": "generation",
            "transition": "transition",
            "registryUid": "registry-uid",
            "registryResourceVersion": "10",
            "requestDigest": self.digest("b"),
            "policyRole": "initial",
            "serviceAccountName": "r-generation-2",
            "creationTargetsSource": "authorized-candidate-policy-projection",
            "creationTargets": creation,
            "managedTargets": managed,
        }

    def test_prepared_result_requires_rust_ordered_closed_targets(self):
        targets = [
            {"namespace": "control", "apiGroup": "", "resource": "serviceaccounts",
             "name": "r-generation-2"},
            {"namespace": "workload", "apiGroup": "apps", "resource": "deployments",
             "name": "api"},
        ]
        value = self.prepared(targets, targets)
        line = demo.PREFIX + json.dumps(value)
        self.assertEqual(demo.result_record(line, "rejection-prepared"), value)
        value["creationTargets"] = list(reversed(targets))
        with self.assertRaises(demo.shared.QualificationError):
            demo.result_record(demo.PREFIX + json.dumps(value), "rejection-prepared")

    def test_missing_capability_preserves_compiler_order(self):
        first = {"namespace": "control", "apiGroup": "", "resource": "secrets"}
        second = {"namespace": "workload", "apiGroup": "", "resource": "configmaps"}
        self.assertEqual(demo.missing_capability(
            {"capabilities": []}, {"capabilities": [first, second]},
        ), first)

    def runner(self):
        runner = object.__new__(demo.CnpgDemoRunner)
        runner.cluster = "owned-test"
        return runner

    @staticmethod
    def resource(api_version, kind, name, api_resource=None):
        value = {
            "apiVersion": api_version, "kind": kind, "name": name, "namespace": "workload",
        }
        if api_resource is not None:
            value["apiResource"] = api_resource
        return value

    def test_denied_targets_are_generation_creates_plus_signed_managed_delta(self):
        runner = self.runner()
        runner.control = "control"
        runner.workload = "workload"
        cluster = self.resource(
            "postgresql.cnpg.io/v1", "Cluster", "database",
            {"plural": "clusters", "scope": "Namespaced", "schemaSha256": self.digest()},
        )
        config = self.resource("v1", "ConfigMap", "api-config")
        deployment = self.resource("apps/v1", "Deployment", "database-api")
        service = self.resource("v1", "Service", "database-api")
        runner.initial_plan = {"ir": {"resources": [cluster, config]}}
        runner.successor_plan = {"ir": {"resources": [cluster, config, deployment, service]}}
        initial = sorted([demo.managed_target(cluster), demo.managed_target(config)], key=demo.target_order)
        successor = sorted(
            [*initial, demo.managed_target(deployment), demo.managed_target(service)],
            key=demo.target_order,
        )
        creates = sorted([
            {"namespace": "control", "apiGroup": "", "resource": "serviceaccounts",
             "name": "r-generation-2"},
            {"namespace": "control", "apiGroup": "apps", "resource": "deployments",
             "name": "r-generation-2"},
        ], key=demo.target_order)
        prepared = self.prepared(creates, successor)
        observed = runner.denied_targets(initial, prepared)
        self.assertEqual(
            {demo.target_key(target) for target in observed},
            {demo.target_key(target) for target in [*creates, demo.managed_target(deployment),
                                                     demo.managed_target(service)]},
        )

    def test_writer_identity_binds_deployment_replicaset_pod_and_container_uids(self):
        runner = self.runner()
        runner.control = "control"
        runner.workload = "workload"
        runner.initial_plan = {"ir": {"resources": [self.resource("v1", "ConfigMap", "config")]}}
        account = "r-generation-1"
        proxy = "proxy-generation-1"
        creation = sorted([
            {"namespace": "control", "apiGroup": "", "resource": "serviceaccounts", "name": account},
            {"namespace": "control", "apiGroup": "apps", "resource": "deployments", "name": account},
            {"namespace": "control", "apiGroup": "apps", "resource": "deployments", "name": proxy},
        ], key=demo.target_order)
        managed = [demo.managed_target(runner.initial_plan["ir"]["resources"][0])]
        prepared = self.prepared(creation, managed)
        prepared["serviceAccountName"] = account
        def deployment(name, uid):
            return {"metadata": {"name": name, "uid": uid}, "spec": {"replicas": 1},
                    "status": {"readyReplicas": 1}}
        deployments = [deployment(account, "deployment-a"), deployment(proxy, "deployment-b")]
        replicasets = [
            {"metadata": {"uid": "set-a", "ownerReferences": [
                {"kind": "Deployment", "uid": "deployment-a", "controller": True}]}},
            {"metadata": {"uid": "set-b", "ownerReferences": [
                {"kind": "Deployment", "uid": "deployment-b", "controller": True}]}},
        ]
        def pod(name, uid, owner, container):
            return {"metadata": {"name": name, "uid": uid, "ownerReferences": [
                        {"kind": "ReplicaSet", "uid": owner, "controller": True}]},
                    "spec": {"nodeName": "node"},
                    "status": {"phase": "Running", "conditions": [
                        {"type": "Ready", "status": "True"}], "containerStatuses": [
                        {"name": "writer", "ready": True, "containerID": container,
                         "restartCount": 0}]}}
        pods = [pod("pod-a", "pod-uid-a", "set-a", "containerd://a"),
                pod("pod-b", "pod-uid-b", "set-b", "containerd://b")]
        runner.k = Mock(side_effect=[json.dumps({"items": deployments}),
                                     json.dumps({"items": replicasets}),
                                     json.dumps({"items": pods})])
        identity = runner.writer_identity(prepared)
        self.assertEqual({item["uid"] for item in identity}, {"deployment-a", "deployment-b"})
        containers = {item["uid"]: item["pods"][0]["containers"] for item in identity}
        self.assertEqual(containers["deployment-a"], [{
            "name": "writer", "containerId": "containerd://a", "restartCount": 0,
        }])

    def test_retained_cluster_cannot_publish_production_evidence(self):
        runner = self.runner()
        runner.disposition = "retained"
        runner.cleanup_confirmed = False
        with tempfile.TemporaryDirectory() as directory:
            runner.output = Path(directory)
            with self.assertRaises(demo.shared.QualificationError):
                runner.publish()
            self.assertFalse((runner.output / "evidence").exists())

    def test_poll_waits_for_absent_resource(self):
        runner = self.runner()
        runner.k = Mock(side_effect=["", '{"metadata":{"uid":"created"}}'])
        with patch.object(demo.time, "sleep"):
            result = runner.wait_object("deployment", "api", "default", 10)
        self.assertEqual(result["metadata"]["uid"], "created")
        self.assertEqual(runner.k.call_count, 2)
        self.assertIn("--ignore-not-found", runner.k.call_args.args)

    def test_poll_times_out_on_absence(self):
        runner = self.runner()
        runner.k = Mock(return_value="")
        with self.assertRaises(demo.shared.QualificationError):
            runner.wait_object("deployment", "api", "default", 0)

    def test_image_identity_requires_verified_index_membership(self):
        runner = self.runner()
        child = "sha256:" + "a" * 64
        raw = json.dumps({"manifests": [{"digest": child}]})
        digest = "sha256:" + hashlib.sha256(raw.encode()).hexdigest()
        reference = "example.invalid/api@" + digest
        runner.command = Mock(return_value=raw)
        self.assertEqual(runner.selected_child_digest(reference, child, "owned-test-control-plane"), child)
        self.assertEqual(runner.selected_child_digest(reference, digest, "owned-test-control-plane"), digest)
        with self.assertRaises(demo.shared.QualificationError):
            runner.selected_child_digest(reference, "sha256:" + "b" * 64, "owned-test-control-plane")
        runner.command = Mock(return_value=raw + " ")
        with self.assertRaises(demo.shared.QualificationError):
            runner.selected_child_digest(reference, child, "owned-test-control-plane")


if __name__ == "__main__":
    unittest.main()
