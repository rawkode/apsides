import argparse
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

import run_cnpg_cli_demo as cli


class CliDemoTests(unittest.TestCase):
    def report(self):
        return {"status": "committed", "artifactTrust": "verified", "releaseQualification": "not-evaluated",
                "releaseDigest": "sha256:" + "a" * 64, "irDigest": "sha256:" + "b" * 64,
                "transition": "t-example-1", "generation": "g-example-1",
                "inventory": {"revision": "1", "snapshotId": "sha256:" + "c" * 64},
                "review": {"unpersisted": "PRIVATE"}}

    def test_report_requires_real_commit_exact_subject_and_inventory(self):
        report = self.report()
        expected = {key: report[key] for key in ["releaseDigest", "irDigest"]}
        self.assertNotIn("PRIVATE", json.dumps(cli.committed_report(json.dumps(report), expected)))
        for key, value in [("status", "pending"), ("releaseDigest", "wrong"), ("irDigest", "wrong"),
                           ("artifactTrust", "unchecked"), ("releaseQualification", "passed"),
                           ("generation", ""), ("inventory", {"revision": "1", "snapshotId": "bad"})]:
            with self.subTest(key=key), self.assertRaises(cli.demo.shared.QualificationError):
                cli.committed_report(json.dumps({**report, key: value}), expected)

    def test_cli_uses_issuer_and_shared_local_store_without_driver_settings(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = cli.CliDemoRunner.__new__(cli.CliDemoRunner)
            runner.output = Path(directory)
            runner.aps = Path('/fixture/aps')
            runner.issuer = Path('/fixture/issuer.kubeconfig')
            runner.inventory = Path('/fixture/state.db')
            runner.refresh_issuer = Mock()
            runner.require_inputs_unchanged = Mock()
            runner.reports = []
            report = self.report()
            runner.command = Mock(return_value=json.dumps(report))
            with patch.dict(os.environ, {"KUBECONFIG": "/wrong/admin", "REAKT_INVENTORY_TOKEN": "PRIVATE",
                                         "APS_PERMISSION_UPGRADE_STEP": "wrong"}):
                runner.deploy_cli(Path('/fixture/candidate'), report)
            command = runner.command.call_args
            self.assertEqual(command.args[0], [runner.aps, "deploy", "--artifact", Path('/fixture/candidate'),
                                              "--executor", "cnpg-cli-demo"])
            env = command.kwargs['environment']
            self.assertEqual(env['KUBECONFIG'], str(runner.issuer))
            self.assertEqual(env['REAKT_INVENTORY_DATABASE'], str(runner.inventory))
            self.assertNotIn('REAKT_INVENTORY_TOKEN', env)
            self.assertNotIn('APS_PERMISSION_UPGRADE_STEP', env)
            runner.refresh_issuer.assert_called_once()
            self.assertEqual(runner.require_inputs_unchanged.call_count, 2)
            self.assertNotIn('PRIVATE', (runner.output/'cli-reports.json').read_text())

    def test_constructor_unifies_stores_and_freezes_cli_source(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory)/'aps'
            binary.write_bytes(b'fixture')
            args = argparse.Namespace(aps=binary)
            def initialize(runner, args):
                runner.inventory = Path(directory)/'inventory.db'
                runner.coordination = Path(directory)/'coordination.db'
                runner.failure_reasons = set()
            with patch.object(cli.demo.CnpgDemoRunner, '__init__', initialize), patch.object(cli.CliDemoRunner, 'freeze_file') as freeze:
                runner = cli.CliDemoRunner(args)
            self.assertEqual(runner.coordination, runner.inventory)
            self.assertFalse(args.collect_performance)
            self.assertFalse(args.retain_on_success)
            self.assertTrue(args.check_input_reactions)
            self.assertEqual([call.args[0] for call in freeze.call_args_list], ['executable/aps', 'producer/cnpg-cli-demo.py'])

    def test_failed_deploy_still_cleans_owned_cluster_without_publication(self):
        runner = cli.CliDemoRunner.__new__(cli.CliDemoRunner)
        runner.prepare = Mock()
        runner.create_cluster = Mock()
        runner.bootstrap = Mock()
        runner.measure = Mock(side_effect=RuntimeError('PRIVATE'))
        runner.finish = Mock()
        runner.publish = Mock()
        runner.failure_reasons = set()
        with self.assertRaises(RuntimeError):
            runner.run()
        runner.finish.assert_called_once_with(False)
        runner.publish.assert_not_called()
        self.assertNotIn('PRIVATE', json.dumps(runner.primary_failure))


if __name__ == '__main__':
    unittest.main()
