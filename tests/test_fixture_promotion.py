import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import run_fixture_promotion as runner

A = 'sha256:' + 'a' * 64
B = 'sha256:' + 'b' * 64


def record():
    return {
        'schemaVersion': 'apsides.dev/fixture-promotion-result/v1', 'passed': True,
        **{role + field: digest for role, digest in [('production', A), ('fixture', B)]
           for field in ['ReleaseDigest', 'IrDigest', 'BinaryDigest', 'ImageDigest']},
    }


def manifest(digest):
    return {'irDigest': digest, 'binaryDigest': digest,
            'image': {'reference': 'registry.example/test@' + digest, 'platform': 'linux/arm64'}}


class FixtureResultTests(unittest.TestCase):
    def test_accepts_one_exact_record_and_binds_both_roles(self):
        value = record()
        parsed = runner.result_record('running 1 test\nAPS_FIXTURE_PROMOTION_RESULT=' + json.dumps(value))
        self.assertEqual(parsed, value)
        runner.validate_result(parsed, manifest(A), manifest(B), A, B)

    def test_rejects_missing_duplicate_and_open_records(self):
        value = record()
        line = 'APS_FIXTURE_PROMOTION_RESULT=' + json.dumps(value)
        for output in ['', line + '\n' + line, 'APS_FIXTURE_PROMOTION_RESULT={"passed":true}']:
            with self.subTest(output=output), self.assertRaises(runner.shared.QualificationError):
                runner.result_record(output)
        for key, replacement in [('passed', 1), ('passed', False), ('schemaVersion', 'wrong'),
                                 ('fixtureIrDigest', 'sha256:bad'), ('extra', 'ignored')]:
            changed = {**value, key: replacement}
            with self.subTest(key=key), self.assertRaises(runner.shared.QualificationError):
                runner.result_record('APS_FIXTURE_PROMOTION_RESULT=' + json.dumps(changed))

    def test_rejects_each_wrong_subject_digest(self):
        for key in record():
            if key.endswith('Digest'):
                changed = record()
                changed[key] = B if changed[key] == A else A
                with self.subTest(key=key), self.assertRaises(runner.shared.QualificationError):
                    runner.validate_result(changed, manifest(A), manifest(B), A, B)

    def test_rejects_duplicate_json_fields(self):
        encoded = json.dumps(record()).replace('"passed": true', '"passed": true, "passed": true')
        with self.assertRaises(runner.shared.QualificationError):
            runner.result_record('APS_FIXTURE_PROMOTION_RESULT=' + encoded)


class FixtureStageTests(unittest.TestCase):
    def test_stage_cannot_be_rebound_independently_of_signed_production(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            production, fixture, stage = [root / name for name in ['production', 'fixture', 'stage']]
            for path in [production, fixture, stage]:
                path.mkdir()
            p = {'testOnly': False, 'resources': []}
            f = {**p, 'testOnly': True}
            overlay = {'schemaVersion': 'apsides.dev/fixture-overlay/v1', 'testOnly': True,
                       'productionReady': False,
                       'productionIrSha256': runner.shared.digest_bytes(runner.shared.canonical(p))[7:],
                       'fixtureIrSha256': runner.shared.digest_bytes(runner.shared.canonical(f))[7:]}
            contents = {'ir.json': p, 'fixture-ir.json': f, 'fixture-overlay.json': overlay}
            for path in [production, fixture, stage]:
                for name, value in contents.items():
                    (path / name).write_bytes(runner.shared.canonical(f if path == fixture and name == 'ir.json' else value))
                if path != stage:
                    files = {name: runner.shared.digest_file(path / name) for name in contents}
                    (path / 'release.json').write_text(json.dumps({
                        'schemaVersion': 'apsides.dev/release/v1',
                        'mode': 'production' if path == production else 'fixture',
                        'files': files, 'source': {'inputsDigest': A},
                        'image': {'reference': 'registry.example/test@' + A},
                        'irDigest': runner.shared.digest_file(path / 'ir.json'),
                        'compilerDigest': A, 'binaryDigest': A,
                    }))
            runner.validate_projection(production, fixture, stage)
            for name in ['fixture-ir.json', 'fixture-overlay.json']:
                original = (stage / name).read_bytes()
                # Even equivalent JSON must retain the signed compiler-stage bytes.
                changed = original + b' '
                (stage / name).write_bytes(changed)
                (fixture / name).write_bytes(changed)
                try:
                    with self.subTest(name=name), self.assertRaises(runner.shared.QualificationError):
                        runner.validate_projection(production, fixture, stage)
                finally:
                    (stage / name).write_bytes(original)
                    (fixture / name).write_bytes(original)


class FixtureRunnerBoundaryTests(unittest.TestCase):
    def test_cargo_must_identify_exactly_one_test_executable(self):
        row = {'reason': 'compiler-artifact', 'target': {'name': 'fixture_promotion', 'kind': ['test']},
               'executable': '/tmp/fixture-driver'}
        self.assertEqual(runner.compiler_artifact(json.dumps(row)), Path('/tmp/fixture-driver'))
        wrong_kind = {**row, 'target': {**row['target'], 'kind': ['bin']}}
        for output in ['', 'not-json', json.dumps(row) + '\n' + json.dumps(row), json.dumps(wrong_kind)]:
            with self.subTest(output=output), self.assertRaises(runner.shared.QualificationError):
                runner.compiler_artifact(output)

    def test_each_frozen_input_role_rejects_changed_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            files = {role: Path(directory) / role for role in
                     ['production', 'fixture', 'trust', 'stage', 'source', 'executable']}
            for path in files.values():
                path.write_bytes(b'original')
            snapshot = runner.snapshot_files(files)
            runner.require_unchanged(files, snapshot)
            for role, path in files.items():
                path.write_bytes(b'changed')
                with self.subTest(role=role), self.assertRaises(runner.shared.QualificationError):
                    runner.require_unchanged(files, snapshot)
                path.write_bytes(b'original')
            files['stage'].unlink()
            files['stage'].symlink_to(files['production'])
            with self.assertRaises(runner.shared.QualificationError):
                runner.require_unchanged(files, snapshot)


class FixturePublicationTests(unittest.TestCase):
    def test_measurement_failure_never_calls_publish(self):
        instance = mock.Mock()
        instance.run_driver.side_effect = runner.shared.QualificationError('rejected')
        with mock.patch.object(runner, 'FixturePromotionRunner', return_value=instance), \
                mock.patch('sys.stderr'):
            status = runner.main(['--artifact', '/tmp/b', '--fixture-artifact', '/tmp/f',
                                  '--fixture-source', '/tmp/source', '--trust-policy', '/tmp/trust',
                                  '--output', '/tmp/output'])
        self.assertEqual(status, 1)
        instance.publish.assert_not_called()

    def test_final_check_rejection_leaves_no_published_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            instance = runner.FixturePromotionRunner.__new__(runner.FixturePromotionRunner)
            instance.output = Path(directory)
            instance.final_check = mock.Mock(side_effect=runner.shared.QualificationError('changed'))
            with self.assertRaises(runner.shared.QualificationError):
                instance.publish()
            self.assertFalse((instance.output / 'evidence').exists())
            self.assertFalse((instance.output / '.fixture-promotion-evidence').exists())


    def test_post_staging_drift_removes_unpublished_receipt(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'runner.py'
            source.write_text('# test source\n')
            instance = runner.FixturePromotionRunner.__new__(runner.FixturePromotionRunner)
            instance.output = root / 'output'
            instance.output.mkdir()
            instance.repository = root
            instance.commit = 'a' * 40
            instance.production_release_digest, instance.fixture_release_digest = A, B
            instance.production_manifest = {
                **manifest(A), 'compilerDigest': A, 'source': {'inputsDigest': A},
                'files': {name: A for name in ['build-inputs.json', 'controller.oci.tar', 'apsides-controller']},
            }
            instance.fixture_manifest = manifest(B)
            instance.result = record()
            instance.driver_binary_digest = A
            instance.source_digests = {'fixture-promotion-runner.py': runner.shared.digest_file(source)}
            instance.snapshot = {"trust/policy": A}
            instance.final_check = mock.Mock(side_effect=[None, runner.shared.QualificationError('changed')])
            with mock.patch.object(runner, 'SOURCE_ARCHIVES', {source: 'fixture-promotion-runner.py'}):
                with self.assertRaises(runner.shared.QualificationError):
                    instance.publish()
            self.assertEqual(list(instance.output.iterdir()), [])


    def test_final_check_binds_measured_result_after_input_check(self):
        instance = runner.FixturePromotionRunner.__new__(runner.FixturePromotionRunner)
        instance.require_inputs_unchanged = mock.Mock()
        instance.production_manifest, instance.fixture_manifest = manifest(A), manifest(B)
        instance.production_release_digest, instance.fixture_release_digest = A, B
        instance.result = record()
        instance.final_check()
        instance.require_inputs_unchanged.assert_called_once()
        instance.result['fixtureReleaseDigest'] = A
        with self.assertRaises(runner.shared.QualificationError):
            instance.final_check()


if __name__ == '__main__':
    unittest.main()
