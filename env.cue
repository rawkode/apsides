package cuenv

import "github.com/cuenv/cuenv/schema"

schema.#Project & {name: "apsides"}

tasks: {
	performanceTest: {command: "python3", args: ["tests/performance/test_performance.py"]}
	authoringEditorCheck: {
		dependsOn: [build, install]
		command: "python3"
		args: ["tests/run_authoring_editor.py", "--aps", "target/debug/aps"]
	}
	packageCli: {command: "python3", args: ["scripts/package_cli.py", "--output", "work/aps-distribution", "--target-dir", "work/cli-package-target"]}
	packageCliTest: {command: "python3", args: ["tests/test_package_cli.py"]}
	cnpgDemoTest: {command: "python3", args: ["tests/test_cnpg_demo.py"]}
	cnpgCliDemoTest: {command: "python3", args: ["tests/test_cnpg_cli_demo.py"]}
	cnpgCleanupTest: {command: "python3", args: ["tests/test_cnpg_cleanup.py"]}
	cnpgCleanupEvidenceTest: {command: "python3", args: ["tests/test_cnpg_cleanup_evidence.py"]}
	uncertainWriterTest: {command: "python3", args: ["tests/test_uncertain_writer.py"]}
	apiGapTest: {command: "python3", args: ["tests/test_api_gap.py"]}
	cnpgPhaseRecoveryTest: {command: "python3", args: ["tests/test_cnpg_phase_recovery.py"]}
	cnpgPhaseRecoveryPackagingTest: {command: "python3", args: ["tests/test_cnpg_phase_recovery_packaging.py"]}
	apiGapProbesTest: {command: "python3", args: ["tests/test_cnpg_credential_probes.py"]}
	crdReactorTest: {command: "python3", args: ["tests/test_cluster_crds.py"]}
	cnpgCleanupTypes: [
		{
			dependsOn: [install]
			command: "deno"
			args: ["run", "--allow-all", "packages/compiler/src/crds.ts", "https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/v1.30.0/config/crd/bases/postgresql.cnpg.io_clusters.yaml", "Cluster", "examples/cnpg/cleanup/.aps/crds/cnpg.ts", "v1"]
		},
		{
			command: "deno"
			args: ["run", "--allow-all", "packages/compiler/src/crds.ts", "https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/v1.30.0/config/crd/bases/postgresql.cnpg.io_backups.yaml", "Backup", "examples/cnpg/cleanup/.aps/crds/backup.ts", "v1"]
		},
	]
	cnpgInitialTypes: {
		dependsOn: [install]
		command: "deno"
		args: ["run", "--allow-all", "packages/compiler/src/crds.ts", "https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/v1.30.0/config/crd/bases/postgresql.cnpg.io_clusters.yaml", "Cluster", "examples/cnpg/initial/.aps/crds/cnpg.ts", "v1"]
	}
	cnpgTypes: {
		dependsOn: [install]
		command: "deno"
		args: ["run", "--allow-all", "packages/compiler/src/crds.ts", "https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/v1.30.0/config/crd/bases/postgresql.cnpg.io_clusters.yaml", "Cluster", "examples/cnpg/.aps/crds/cnpg.ts", "v1"]
	}
	releaseTest: {command: "cargo", args: ["test", "--locked", "-p", "release", "--features", "test-support"]}
	productionEvidenceLive: {command: "cargo", args: ["test", "--locked", "-p", "release", "--lib", "assembled_reproducibility_evidence", "--", "--ignored"]}
	inputIsolationEvidenceLive: {command: "cargo", args: ["test", "--locked", "-p", "release", "--lib", "assembled_input_isolation_evidence", "--", "--ignored"]}
	reactorEvidenceLive: {command: "cargo", args: ["test", "--locked", "-p", "release", "--lib", "reactor_evidence_live::assembled_reactor_evidence_matches_production_contract", "--", "--ignored", "--exact"]}
	fixturePromotionTest: {command: "python3", args: ["tests/test_fixture_promotion.py"]}
	fixturePromotionEvidence: {command: "python3", args: ["tests/run_fixture_promotion.py"]}
	fixturePromotionLive: {command: "cargo", args: ["test", "--locked", "-p", "release", "--test", "fixture_promotion", "fixture_promotion_step", "--", "--ignored", "--exact", "--nocapture"]}
	permissionUpgradeStep: {command: "cargo", args: ["test", "--locked", "-p", "platform", "--features", "test-support", "--test", "permission_upgrade", "permission_upgrade_step", "--", "--ignored", "--exact", "--nocapture"]}
	releaseCheck: {command: "cargo", args: ["clippy", "--locked", "-p", "release", "--features", "test-support", "--all-targets", "--", "-D", "warnings"]}
	reproducibilityTest: {command: "python3", args: ["-m", "unittest", "tests/test_reproducibility.py"]}
	inputIsolationTest: {command: "python3", args: ["tests/test_input_isolation.py"]}
	inputIsolationProbeTest: {command: "deno", args: ["test", "--allow-all", "tests/isolation_probe.test.mts"]}
	reactorPackagingTest: {command: "python3", args: ["tests/test_reactor_packaging.py"]}
	storageProbeTest: {command: "cargo", args: ["test", "--locked", "-p", "inventory", "--test", "pvc_restart"]}
	storageBuildTest: {command: "python3", args: ["tests/storage/test_build.py"]}
	storageRestartTest: {command: "python3", args: ["tests/storage/test_restart.py"]}
	coordinationRestartTest: {command: "python3", args: ["tests/storage/test_coordination.py"]}
	coordinationProbeTest: {command: "cargo", args: ["test", "--locked", "-p", "coordination", "--test", "pvc_restart"]}
	storageBackupTest: {command: "python3", args: ["tests/storage/test_backup.py"]}
	storageBackup: {command: "python3", args: ["tests/run_storage_backup.py"]}
	coordinationRestart: {command: "python3", args: ["tests/run_coordination_restart.py"]}
	storagePrepare: {command: "python3", args: ["tests/storage/build.py"]}
	storageRestart: {command: "python3", args: ["tests/run_storage_restart.py"]}
	coordinationTest: {command: "cargo", args: ["test", "--locked", "-p", "coordination"]}
	coordinationCheck: [
		{command: "cargo", args: ["clippy", "--locked", "-p", "coordination", "--all-targets", "--", "-D", "warnings"]},
		{command: "cargo", args: ["check", "--locked", "-p", "coordination", "--no-default-features"]},
	]
	registryRuntimeCheck: {command: "cargo", args: ["check", "--locked", "-p", "platform", "--no-default-features"]}
	candidateQualificationTest: {command: "cargo", args: ["test", "--locked", "-p", "platform", "--features", "test-support", "--lib", "candidate_qualification::"]}
	candidateQualificationArchivedTest: {command: "cargo", args: ["test", "--locked", "-p", "platform", "--features", "test-support", "--lib", "tests::candidate_qualification::public_candidate_qualification_rejects_drift_and_reserves_exact_candidate", "--", "--exact", "--ignored"]}
	deploymentCeilingTest: {command: "python3", args: ["-m", "unittest", "tests/test_ceiling_observations.py", "tests/test_deployment_ceiling.py"]}
	ownershipCollisionTest: {command: "python3", args: ["-m", "unittest", "tests/test_ceiling_observations.py", "tests/test_deployment_ceiling.py", "tests/test_ownership_collision.py"]}
	runtimeTest: {command: "cargo", args: ["test", "--locked", "-p", "runtime"]}
	runtimeCheck: {command: "cargo", args: ["clippy", "--locked", "-p", "runtime", "--all-targets", "--", "-D", "warnings"]}
	previewTest: {command: "cargo", args: ["test", "--locked", "-p", "platform", "--features", "test-support", "preview_"]}
	previewIssuedLive: {command: "cargo", args: ["test", "--locked", "-p", "platform", "--features", "test-support", "--lib", "preview_issued_live", "--", "--ignored"]}
	previewQualification: {command: "python3", args: ["tests/run_preview_qualification.py"]}
	previewQualificationTest: {command: "python3", args: ["-m", "unittest", "tests/test_preview_qualification.py"]}
	previewReadOnlyLive: {command: "cargo", args: ["test", "--locked", "-p", "platform", "--features", "test-support", "--test", "preview_readonly", "--", "--ignored"]}
	registryTest: {command: "cargo", args: ["test", "--locked", "-p", "deploy", "-p", "platform", "-p", "cli", "--features", "platform/test-support"]}
	registryCheck: {command: "cargo", args: ["clippy", "--locked", "-p", "deploy", "-p", "platform", "-p", "cli", "--features", "platform/test-support", "--all-targets", "--", "-D", "warnings"]}
	authorCompilerTest: {
		dependsOn: [install]
		command: "deno"
		env: {CI: "1"}
		args: ["task", "--filter", "@apsides/compiler", "test"]
	}
	authorCheck: {
		dependsOn: [install]
		command: "deno"
		env: {CI: "1"}
		args: ["task", "--filter", "@apsides/kubernetes", "typecheck"]
	}
	authorTest: {
		dependsOn: [install]
		command: "deno"
		env: {CI: "1"}
		args: ["task", "--filter", "@apsides/kubernetes", "test"]
	}
	docsInstall: {command: "deno", args: ["task", "--cwd", "docs", "install"], env: {CI: "1"}}
	docsContent: {command: "deno", args: ["task", "--cwd", "docs", "prepare:content"]}
	docsBuild: {
		dependsOn: [docsInstall, docsContent]
		command: "deno"
		args: ["task", "--cwd", "docs", "build"]
	}
	docsDev: {
		dependsOn: [docsInstall, docsContent]
		command: "deno"
		args: ["task", "--cwd", "docs", "dev"]
	}
	install: {command: "deno", args: ["install", "--frozen"], env: {CI: "1"}}
	build: {command: "cargo", args: ["build", "--locked", "-p", "cli"]}
}
