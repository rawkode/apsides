import json
from pathlib import Path
import types
import unittest
from unittest.mock import Mock, patch

import run_private_pull as p


class PrivatePullTests(unittest.TestCase):
    def test_only_fresh_authentication_failure_is_accepted(self):
        pod={'metadata':{'uid':'pod'},'status':{'containerStatuses':[{'name':'controller','state':{'waiting':{'reason':'ErrImagePull'}}}]}}
        event={'metadata':{'uid':'event'},'involvedObject':{'uid':'pod'},'count':2,'reason':'Failed','message':'private details: failed to authorize: 401 Unauthorized'}
        result=p.authentication_failure(pod,[event],{'event':1})
        self.assertEqual(result['reason'],'registry-authentication-rejected')
        self.assertNotIn('private',json.dumps(result))
        self.assertIsNone(p.authentication_failure(pod,[event],{'event':2}))
        event['message']='x509 certificate unknown authority'
        self.assertIsNone(p.authentication_failure(pod,[event],{}))
        event['message']='failed to authorize';event['involvedObject']['uid']='other'
        self.assertIsNone(p.authentication_failure(pod,[event],{}))

    def test_running_without_valid_auth_fails(self):
        with self.assertRaises(p.shared.QualificationError):
            p.authentication_failure({'status':{'containerStatuses':[{'state':{'running':{ 'startedAt':'now'}}}]}},[],{})

    def runner(self):
        r=p.PrivatePullRunner.__new__(p.PrivatePullRunner)
        r.cluster='owned';r.control='control';r.args=types.SimpleNamespace(image_pull_secret=['qualification-registry'])
        r.initial_manifest={'image':{'reference':'registry/image@sha256:manifest'}}
        r.successor_manifest={'image':{'reference':'registry/next@sha256:next'}}
        r.registry=types.SimpleNamespace(publishedrefs={'registry/image@sha256:manifest':{'configDigest':'sha256:config'},'registry/next@sha256:next':{'configDigest':'sha256:nextconfig'}})
        return r

    def test_cached_config_or_manifest_rejects(self):
        r=self.runner()
        for content in ['sha256:config','sha256:manifest','sha256:next']:
            r.command=Mock(return_value=content)
            with self.assertRaises(p.shared.QualificationError):r.image_absent()
        r.command=Mock(return_value='sha256:unrelated');r.image_absent()

    def test_credential_update_is_uid_rv_guarded(self):
        r=self.runner();old={'metadata':{'uid':'uid','resourceVersion':'2'},'data':{'config':'invalid'}}
        new={'metadata':{'uid':'uid','resourceVersion':'3'},'data':{'config':'valid'}}
        r.registry.secret_object=Mock(return_value={'metadata':{'name':'qualification-registry','namespace':'control'},'type':'kubernetes.io/dockerconfigjson','data':new['data']})
        r.secret=Mock(side_effect=[old,new]);r.k=Mock()
        self.assertEqual(r.install_credentials(True,old),new)
        patch=json.loads(r.k.call_args.kwargs['data'])
        self.assertIn('--patch-file',r.k.call_args.args)
        self.assertNotIn('valid',json.dumps(r.k.call_args.args))
        self.assertEqual(patch[:2],[{'op':'test','path':'/metadata/uid','value':'uid'},{'op':'test','path':'/metadata/resourceVersion','value':'2'}])

    def test_concurrent_credentials_not_overwritten(self):
        r=self.runner();r.registry.secret_object=Mock(return_value={'metadata':{'name':'qualification-registry','namespace':'control'},'type':'kubernetes.io/dockerconfigjson','data':{}})
        old={'metadata':{'uid':'old'},'data':{}}
        r.secret=Mock(return_value={'metadata':{'uid':'replacement'},'data':{}});r.k=Mock()
        with self.assertRaises(p.shared.QualificationError):r.install_credentials(True,old)
        r.k.assert_not_called()

    def test_no_candidate_archive_is_loaded(self):
        r=self.runner();r.node_platform='linux/arm64';r.registry.host='registry';r.output=Path('/unused')
        for m in [r.initial_manifest,r.successor_manifest]:m['image']['platform']=r.node_platform
        r.profile={'envoyImage':'public/envoy','adminImage':'public/admin'}
        r.command=Mock();r.freeze_file=Mock();r.import_archive=Mock();r.require_inputs_unchanged=Mock()
        r.load_images()
        self.assertEqual([c.args[1] for c in r.import_archive.call_args_list],['public/envoy','public/admin'])

    def test_noninitial_lifecycle_keeps_existing_driver(self):
        r=self.runner()
        with patch.object(p.demo.CnpgDemoRunner,'lifecycle',return_value={'real':'result'}) as inherited:
            self.assertEqual(r.lifecycle('activate-upgrade','transition'),{'real':'result'})
            inherited.assert_called_once_with('activate-upgrade','transition',360)

    def test_publication_requires_success_and_cleanup(self):
        r=self.runner();r.pull_observation=None
        with self.assertRaises(p.shared.QualificationError):r.publish()


if __name__=='__main__':unittest.main()
