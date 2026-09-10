"""Private-registry qualification through real platform staging, without release evidence.

The registry fixture owns TLS, publication and credentials. Construct
PrivatePullRunner(args, registry), then run(). The fixture must provide host,
ca_path, publishedrefs (reference -> manifestDigest/configDigest), install_node_trust(runner),
and secret_object(namespace, valid). It must remove its private material afterward.
"""
from contextlib import redirect_stdout
import io
import inspect
import json
from pathlib import Path
import time

import run_cnpg_demo as demo

shared = demo.shared


def pull_diagnostic(pod, events):
    """Project only closed classifications; never persist Kubernetes messages."""
    statuses = (pod or {}).get('status', {}).get('containerStatuses', [])
    waiting = {s.get('state', {}).get('waiting', {}).get('reason') for s in statuses}
    messages = [e.get('message', '').lower() for e in events
                if pod and e.get('involvedObject', {}).get('uid') == pod['metadata']['uid']]
    return {
        'podPresent': pod is not None,
        'imagePullError': bool(waiting & {'ErrImagePull', 'ImagePullBackOff'}),
        'authenticationRequired': any('no basic auth credentials' in m for m in messages),
        'http401': any('401 unauthorized' in m or '401 (unauthorized)' in m or 'unexpected status: 401' in m for m in messages),
        'certificateError': any('x509:' in m for m in messages),
        'dnsError': any('no such host' in m for m in messages),
        'connectionRefused': any('connection refused' in m for m in messages),
    }


def require(ok, reason):
    if not ok:
        raise shared.QualificationError(reason)


def authentication_failure(pod, events, previous, *, allow_missing=False):
    statuses = pod.get('status', {}).get('containerStatuses', [])
    require(not any(s.get('state', {}).get('running') for s in statuses), 'private image unexpectedly started without valid credentials')
    waiting = any(s.get('name') == 'controller' and s.get('state', {}).get('waiting', {}).get('reason') in {'ErrImagePull', 'ImagePullBackOff'} for s in statuses)
    for event in events:
        metadata = event.get('metadata', {})
        count = event.get('count', 1)
        message = event.get('message', '').lower()
        rejection = None
        if any(text in message for text in ['401 unauthorized', '401 (unauthorized)', 'unexpected status: 401']):
            rejection = 'registry-authentication-rejected'
        elif allow_missing and 'no basic auth credentials' in message:
            rejection = 'registry-credentials-missing'
        if (waiting and event.get('involvedObject', {}).get('uid') == pod['metadata']['uid']
                and event.get('reason') == 'Failed' and type(count) is int and count > previous.get(metadata.get('uid'), 0)
                and rejection is not None):
            return {'eventUid': metadata['uid'], 'count': count, 'podUid': pod['metadata']['uid'], 'reason': rejection}
    return None


class PrivatePullRunner(demo.CnpgDemoRunner):
    def __init__(self, args, registry):
        require(len(getattr(args, 'image_pull_secret', [])) == 1, 'private pull requires one platform Secret reference')
        args.retain_on_success = False
        args.collect_performance = False
        args.check_input_reactions = False
        self.registry = registry
        super().__init__(args)
        self.failure_reasons.add('private registry authentication rejection was not observed')
        source = Path(__file__).resolve()
        require(shared.source_state(source) == (self.repository, self.commit), 'private pull source is not frozen')
        self.sources.append(source)
        self.source_archives[source] = 'private-pull-runner.py'
        self.source_digests[source] = shared.digest_file(source)
        self.freeze_file('producer/private-pull-runner.py', source)
        registry_source = Path(inspect.getfile(type(registry))).resolve()
        require(shared.source_state(registry_source) == (self.repository, self.commit), 'registry helper source is not frozen')
        self.sources.append(registry_source)
        self.source_archives[registry_source] = 'private-registry.py'
        self.source_digests[registry_source] = shared.digest_file(registry_source)
        self.freeze_file('producer/private-registry.py', registry_source)
        self.freeze_file('registry/public-ca', registry.ca_path)
        self.pull_observation = None

    def run(self):
        with redirect_stdout(io.StringIO()):
            super().run()
        path = self.output / 'result.json'
        result = json.loads(path.read_text())
        result['scope'] = 'Local private-registry qualification through platform staging and CNPG upgrade'
        result['productionEvidence'] = None
        result['productionEvidenceStatus'] = 'not-produced-private-pull-qualification-only'
        path.write_bytes(shared.canonical(result))
        print(json.dumps(result))

    def create_cluster(self):
        super().create_cluster()
        self.registry.install_node_trust(self)

    def load_images(self):
        for manifest in [self.initial_manifest, self.successor_manifest]:
            reference = manifest['image']['reference']
            require(manifest['image']['platform'] == self.node_platform and reference.startswith(self.registry.host + '/')
                    and reference in self.registry.publishedrefs, 'private candidate publication differs')
        for index, reference in enumerate([self.profile['envoyImage'], self.profile['adminImage']]):
            self.command(['docker', 'pull', reference], timeout=300)
            archive = self.output / f'proxy-{index}.tar'
            self.command(['docker', 'image', 'save', '--output', archive, reference], timeout=300)
            self.freeze_file(f'proxy/archive-{index}', archive)
            self.import_archive(archive, reference)
        self.require_inputs_unchanged()

    def image_absent(self):
        content = set(self.command(['docker', 'exec', self.cluster+'-control-plane', 'ctr', '-n', 'k8s.io', 'content', 'ls', '-q']).splitlines())
        for manifest in [self.initial_manifest, self.successor_manifest]:
            ref = manifest['image']['reference']
            require(ref.rsplit('@', 1)[1] not in content and self.registry.publishedrefs[ref]["configDigest"] not in content, 'private candidate image is already cached')

    def pinned_workload(self, name, previous=None):
        deployment = json.loads(self.k('get', 'deployment', name, '-n', self.control, '-o', 'json'))
        spec = deployment['spec']['template']['spec']
        require(spec.get('imagePullSecrets') == [{'name': self.args.image_pull_secret[0]}]
                and spec['containers'][0]['image'] == self.initial_manifest['image']['reference'], 'private staged Deployment differs from approved image or pull reference')
        binding = {'deploymentUid': deployment['metadata']['uid'], 'templateDigest': shared.digest_bytes(shared.canonical(deployment['spec']['template'])),
                   'policyDigest': shared.digest_bytes(shared.canonical(json.loads(self.k('get', 'configmap', 'apsides-policy-'+self.domain, '-n', self.control, '-o', 'json'))))}
        require(binding['deploymentUid'] == self.private_deployment_uid, 'private Deployment UID differs from protected stage')
        require(previous is None or binding == previous, 'private pull changed protected Deployment or policy')
        pods = json.loads(self.k('get', 'pods', '-n', self.control, '-o', 'json'))['items']
        pods = [p for p in pods if p.get('spec', {}).get('serviceAccountName') == spec['serviceAccountName']]
        require(len(pods) <= 1, 'private runtime Pod population is ambiguous')
        if pods:
            require(pods[0]['spec'].get('imagePullSecrets') == spec['imagePullSecrets'], 'private Pod pull references differ')
        return binding, pods[0] if pods else None

    def wait_auth_failure(self, name, binding, previous, *, allow_missing=False):
        deadline = time.monotonic()+180
        while True:
            _, pod = self.pinned_workload(name, binding)
            events = []
            if pod:
                events = json.loads(self.k('get', 'events', '-n', self.control, '-o', 'json'))['items']
                result = authentication_failure(pod, events, previous, allow_missing=allow_missing)
                if result:
                    return result
            if time.monotonic() >= deadline:
                (self.output / 'private-pull-diagnostic.json').write_bytes(
                    shared.canonical({'phase': self.phase, **pull_diagnostic(pod, events)}))
                raise shared.QualificationError('private registry authentication rejection was not observed')
            time.sleep(2)

    def secret(self):
        return json.loads(self.k('get', 'secret', self.args.image_pull_secret[0], '-n', self.control, '-o', 'json'))

    def install_credentials(self, valid, previous=None):
        value = self.registry.secret_object(self.control, valid=valid)
        require(value['metadata']['name'] == self.args.image_pull_secret[0] and value['metadata']['namespace'] == self.control
                and value['type'] == 'kubernetes.io/dockerconfigjson', 'private credential fixture reference differs')
        if previous is None:
            self.k('create', '-f', '-', data=json.dumps(value))
        else:
            current = self.secret()
            require(current['metadata']['uid'] == previous['metadata']['uid'] and current['data'] == previous['data'], 'private credential fixture changed concurrently')
            patch = [{'op':'test','path':'/metadata/uid','value':current['metadata']['uid']},
                     {'op':'test','path':'/metadata/resourceVersion','value':current['metadata']['resourceVersion']},
                     {'op':'replace','path':'/data','value':value['data']}]
            self.k('patch', 'secret', self.args.image_pull_secret[0], '-n', self.control, '--type=json', '--patch-file', '/dev/stdin', '-o', 'name', data=json.dumps(patch))
        result = self.secret()
        require(result['data'] == value['data'] and (previous is None or result['metadata']['uid'] == previous['metadata']['uid']), 'private credential update was not preserved')
        return result

    def lifecycle(self, action, transition, timeout=360):
        if action != 'activate-initial':
            return super().lifecycle(action, transition, timeout)
        self.phase = 'private-pull-missing-auth'
        self.image_absent()
        require(not self.k('get', 'secret', self.args.image_pull_secret[0], '-n', self.control, '--ignore-not-found', '-o', 'name').strip(), 'private credential Secret already exists')
        completed, pending, _ = demo.LIFECYCLE[action]
        result = self.step(action, {completed, pending}, self.issuer, transition=transition)
        require(result['transition'] == transition, 'private activation changed transition')
        generation = result['generation']
        registry_object = json.loads(self.k('get', 'configmap', 'apsides-registry-'+self.workload, '-n', self.control, '-o', 'json'))
        registry = json.loads(registry_object['data']['state.json'])
        stored = registry['transitions'][transition]
        name = stored['runtimeBinding']['serviceAccountName']
        require(stored['runtimeBinding']['generationId'] == generation and stored['inputs']['allocation']['artifactDigest'] == self.pair['candidates']['initialReleaseDigest'], 'private runtime binding differs from initial candidate')
        self.private_deployment_uid = stored['stagedIdentityUids']['apps/v1/Deployment/'+self.control+'/'+name]
        binding, _ = self.pinned_workload(name)
        missing = self.wait_auth_failure(name, binding, {}, allow_missing=True)
        self.phase = 'private-pull-invalid-auth'
        invalid = self.install_credentials(False)
        events = json.loads(self.k("get", "events", "-n", self.control, "-o", "json"))["items"]
        baseline = {e["metadata"]["uid"]: e.get("count", 1) for e in events}
        denied = self.wait_auth_failure(name, binding, baseline)
        require(denied['podUid'] == missing['podUid'], 'private runtime Pod changed during failed pulls')
        self.phase = 'private-pull-valid-auth'
        valid = self.install_credentials(True, invalid)
        self.command(['kubectl', '--kubeconfig', self.admin, 'rollout', 'status', 'deployment/'+name, '-n', self.control, '--timeout=300s'], timeout=320)
        _, pod = self.pinned_workload(name, binding)
        require(pod is not None and pod['metadata']['uid'] == missing['podUid'] and any(s.get('name') == 'controller' and s.get('ready') and s.get('state', {}).get('running') for s in pod.get('status', {}).get('containerStatuses', [])), 'private controller did not become ready')
        status = next(s for s in pod['status']['containerStatuses'] if s['name'] == 'controller')
        selected = self.registry.publishedrefs[self.initial_manifest['image']['reference']]
        require(status.get('imageID', '').rsplit('@', 1)[-1] in {selected['manifestDigest'], selected['configDigest']}, 'private runtime image identity differs')
        self.pull_observation = {'passed':True, **binding, 'missing':missing, 'invalid':denied, 'secretUid':valid['metadata']['uid'], 'podUid':pod['metadata']['uid'], 'image':self.initial_manifest['image']['reference']}
        return super().lifecycle(action, transition, timeout)

    def publish(self):
        require(self.pull_observation is not None and self.cleanup_confirmed and self.disposition == 'deleted'
                and not self.admin.exists() and not self.issuer.exists(), 'private pull qualification requires complete cleanup')
        self.require_inputs_unchanged()
        self.verify_docker()
        result = {'schemaVersion':'apsides.dev/private-pull-qualification/v1', 'passed':True, 'releaseVerified':False,
                  'productionEvidence':False, 'runnerSourceCommit':self.commit, 'frozenInputDigests':self.snapshot,
                  'candidatePair':self.pair['candidates'], 'observation':self.pull_observation,
                  'scope':'Missing Secret, rejection while invalid credentials exist, then valid credentials through platform-staged initial runtime; ordinary CNPG upgrade follows.',
                  'limitation':'Invalid-phase HTTP401 does not establish when kubelet consumed the changed Secret; no cache-bypass or production-release claim.'}
        path = self.output/'private-pull-qualification.json'
        path.write_bytes(shared.canonical(result))
        return None
