package cuenv

import "github.com/cuenv/cuenv/schema"

schema.#Project & {name: "apsides"}

tasks: {
	authoringEditorCheck: {
		dependsOn: [build, install]
		command: "python3"
		args: ["tests/reakt/run_authoring_editor.py", "--aps", "target/debug/aps"]
	}
	packageCli: {command: "python3", args: ["scripts/reakt/package_cli.py", "--output", "work/aps-distribution", "--target-dir", "work/cli-package-target"]}
	packageCliTest: {command: "python3", args: ["tests/reakt/test_package_cli.py"]}
	cnpgDemoTest: {command: "python3", args: ["tests/reakt/test_cnpg_demo.py"]}
	cnpgCleanupTest: {command: "python3", args: ["tests/reakt/test_cnpg_cleanup.py"]}
	cnpgCleanupEvidenceTest: {command: "python3", args: ["tests/reakt/test_cnpg_cleanup_evidence.py"]}
	crdReactorTest: {command: "python3", args: ["tests/reakt/test_cluster_crds.py"]}
	cnpgCleanupTypes: [
		{
			dependsOn: [install]
			command: "node"
			args: ["packages/compiler/src/crds.ts", "https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/v1.30.0/config/crd/bases/postgresql.cnpg.io_clusters.yaml", "Cluster", "examples/cnpg/cleanup/.aps/crds/cnpg.ts", "v1"]
		},
		{
			command: "node"
			args: ["packages/compiler/src/crds.ts", "https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/v1.30.0/config/crd/bases/postgresql.cnpg.io_backups.yaml", "Backup", "examples/cnpg/cleanup/.aps/crds/backup.ts", "v1"]
		},
	]
	cnpgInitialTypes: {
		dependsOn: [install]
		command: "node"
		args: ["packages/compiler/src/crds.ts", "https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/v1.30.0/config/crd/bases/postgresql.cnpg.io_clusters.yaml", "Cluster", "examples/cnpg/initial/.aps/crds/cnpg.ts", "v1"]
	}
	cnpgTypes: {
		dependsOn: [install]
		command: "node"
		args: ["packages/compiler/src/crds.ts", "https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/v1.30.0/config/crd/bases/postgresql.cnpg.io_clusters.yaml", "Cluster", "examples/cnpg/.aps/crds/cnpg.ts", "v1"]
	}
	releaseTest: {command: "cargo", args: ["test", "--locked", "-p", "release", "--features", "test-support"]}
	productionEvidenceLive: {command: "cargo", args: ["test", "--locked", "-p", "release", "--lib", "assembled_reproducibility_evidence", "--", "--ignored"]}
	inputIsolationEvidenceLive: {command: "cargo", args: ["test", "--locked", "-p", "release", "--lib", "assembled_input_isolation_evidence", "--", "--ignored"]}
	reactorEvidenceLive: {command: "cargo", args: ["test", "--locked", "-p", "release", "--lib", "reactor_evidence_live::assembled_reactor_evidence_matches_production_contract", "--", "--ignored", "--exact"]}
	fixturePromotionTest: {command: "python3", args: ["tests/reakt/test_fixture_promotion.py"]}
	fixturePromotionEvidence: {command: "python3", args: ["tests/reakt/run_fixture_promotion.py"]}
	fixturePromotionLive: {command: "cargo", args: ["test", "--locked", "-p", "release", "--test", "fixture_promotion", "fixture_promotion_step", "--", "--ignored", "--exact", "--nocapture"]}
	permissionUpgradeStep: {command: "cargo", args: ["test", "--locked", "-p", "platform", "--features", "test-support", "--test", "permission_upgrade", "permission_upgrade_step", "--", "--ignored", "--exact", "--nocapture"]}
	releaseCheck: {command: "cargo", args: ["clippy", "--locked", "-p", "release", "--features", "test-support", "--all-targets", "--", "-D", "warnings"]}
	reproducibilityTest: {command: "python3", args: ["-m", "unittest", "tests/reakt/test_reproducibility.py"]}
	inputIsolationTest: {command: "python3", args: ["tests/reakt/test_input_isolation.py"]}
	inputIsolationProbeTest: {command: "node", args: ["--test", "tests/reakt/isolation_probe.test.mts"]}
	reactorPackagingTest: {command: "python3", args: ["tests/reakt/test_reactor_packaging.py"]}
	lifecycleCeilingTest: {command: "python3", args: ["tests/reakt/test_lifecycle_ceiling.py"]}
	lifecycleCleanupTest: {command: "python3", args: ["tests/reakt/test_lifecycle_cleanup.py"]}
	lifecyclePermissionsTest: {command: "python3", args: ["tests/reakt/test_lifecycle_permissions.py"]}
	lifecycleDiagnosticsTest: {command: "python3", args: ["tests/reakt/test_lifecycle_diagnostics.py"]}
	lifecycleInterruptionTest: {command: "python3", args: ["tests/reakt/test_lifecycle_interruption.py"]}
	storageProbeTest: {command: "cargo", args: ["test", "--locked", "-p", "inventory", "--test", "pvc_restart"]}
	storageBuildTest: {command: "python3", args: ["tests/reakt/storage/test_build.py"]}
	storageRestartTest: {command: "python3", args: ["tests/reakt/storage/test_restart.py"]}
	coordinationRestartTest: {command: "python3", args: ["tests/reakt/storage/test_coordination.py"]}
	coordinationProbeTest: {command: "cargo", args: ["test", "--locked", "-p", "coordination", "--test", "pvc_restart"]}
	storageBackupTest: {command: "python3", args: ["tests/reakt/storage/test_backup.py"]}
	storageBackup: {command: "python3", args: ["tests/reakt/run_storage_backup.py"]}
	coordinationRestart: {command: "python3", args: ["tests/reakt/run_coordination_restart.py"]}
	storagePrepare: {command: "python3", args: ["tests/reakt/storage/build.py"]}
	storageRestart: {command: "python3", args: ["tests/reakt/run_storage_restart.py"]}
	coordinationTest: {command: "cargo", args: ["test", "--locked", "-p", "coordination"]}
	coordinationCheck: [
		{command: "cargo", args: ["clippy", "--locked", "-p", "coordination", "--all-targets", "--", "-D", "warnings"]},
		{command: "cargo", args: ["check", "--locked", "-p", "coordination", "--no-default-features"]},
	]
	registryRuntimeCheck: {command: "cargo", args: ["check", "--locked", "-p", "platform", "--no-default-features"]}
	candidateQualificationTest: {command: "cargo", args: ["test", "--locked", "-p", "platform", "--features", "test-support", "--lib", "candidate_qualification::"]}
	candidateQualificationArchivedTest: {command: "cargo", args: ["test", "--locked", "-p", "platform", "--features", "test-support", "--lib", "tests::candidate_qualification::public_candidate_qualification_rejects_drift_and_reserves_exact_candidate", "--", "--exact", "--ignored"]}
	deploymentCeilingTest: {command: "python3", args: ["-m", "unittest", "tests/reakt/test_ceiling_observations.py", "tests/reakt/test_deployment_ceiling.py"]}
	ownershipCollisionTest: {command: "python3", args: ["-m", "unittest", "tests/reakt/test_ceiling_observations.py", "tests/reakt/test_deployment_ceiling.py", "tests/reakt/test_ownership_collision.py"]}
	runtimeTest: {command: "cargo", args: ["test", "--locked", "-p", "runtime"]}
	runtimeCheck: {command: "cargo", args: ["clippy", "--locked", "-p", "runtime", "--all-targets", "--", "-D", "warnings"]}
	previewTest: {command: "cargo", args: ["test", "--locked", "-p", "platform", "--features", "test-support", "preview_"]}
	previewIssuedLive: {command: "cargo", args: ["test", "--locked", "-p", "platform", "--features", "test-support", "--lib", "preview_issued_live", "--", "--ignored"]}
	previewQualification: {command: "python3", args: ["tests/reakt/run_preview_qualification.py"]}
	previewQualificationTest: {command: "python3", args: ["-m", "unittest", "tests/reakt/test_preview_qualification.py"]}
	previewReadOnlyLive: {command: "cargo", args: ["test", "--locked", "-p", "platform", "--features", "test-support", "--test", "preview_readonly", "--", "--ignored"]}
	registryTest: {command: "cargo", args: ["test", "--locked", "-p", "deploy", "-p", "platform", "-p", "cli", "--features", "platform/test-support"]}
	registryCheck: {command: "cargo", args: ["clippy", "--locked", "-p", "deploy", "-p", "platform", "-p", "cli", "--features", "platform/test-support", "--all-targets", "--", "-D", "warnings"]}
	authorCompilerTest: {
		dependsOn: [install]
		command: "pnpm"
		env: {CI: "1"}
		args: ["--filter", "@apsides/compiler", "test"]
	}
	authorCheck: {
		dependsOn: [install]
		command: "pnpm"
		env: {CI: "1"}
		args: ["--filter", "@reakt/kubernetes", "typecheck"]
	}
	authorTest: {
		dependsOn: [install]
		command: "pnpm"
		env: {CI: "1"}
		args: ["--filter", "@reakt/kubernetes", "test"]
	}
	docsInstall: {command: "npm", args: ["ci", "--prefix", "docs", "--no-audit", "--no-fund"]}
	docsContent: {command: "npm", args: ["--prefix", "docs", "run", "prepare:content"]}
	docsBuild: {
		dependsOn: [docsInstall, docsContent]
		command: "npm"
		args: ["--prefix", "docs", "run", "build"]
	}
	docsDev: {
		dependsOn: [docsInstall, docsContent]
		command: "npm"
		args: ["--prefix", "docs", "run", "dev"]
	}
	install: {command: "pnpm", args: ["install", "--frozen-lockfile"], env: {CI: "1"}}
	build: {command: "cargo", args: ["build", "--locked", "-p", "cli"]}
}
