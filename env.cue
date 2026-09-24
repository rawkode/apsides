package cuenv

import "github.com/cuenv/cuenv/schema"

schema.#Project & {name: "apsides"}

// cuenv runs tasks hermetically by default, in a directory holding only
// their declared inputs. Every task here reads the whole checkout (the Cargo
// workspace, Deno packages, docs and examples), so they run in the workspace.
tasks: [_]: hermetic: false

tasks: {
	authoringEditorCheck: {
		dependsOn: [build, install]
		command: "python3"
		args: ["tests/run_authoring_editor.py", "--aps", "target/debug/aps"]
	}
	packageCli: {command: "python3", args: ["scripts/package_cli.py", "--output", "work/aps-distribution", "--target-dir", "work/cli-package-target"]}
	packageCliTest: {command: "python3", args: ["tests/test_package_cli.py"]}
	packagingTest: {command: "python3", args: ["tests/test_packaging.py"]}
	cnpgCleanupTypes: {
		dependsOn: [install]
		command: "sh"
		args: ["-c", "deno run --allow-all packages/compiler/src/crds.ts https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/v1.30.0/config/crd/bases/postgresql.cnpg.io_clusters.yaml Cluster examples/cnpg/cleanup/.aps/crds/cnpg.ts v1 && deno run --allow-all packages/compiler/src/crds.ts https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/v1.30.0/config/crd/bases/postgresql.cnpg.io_backups.yaml Backup examples/cnpg/cleanup/.aps/crds/backup.ts v1"]
	}
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
	// Compiles the CNPG initial and upgraded examples and exercises `aps
	// diff` between them: a clean diff (no policy) exits 0, and a policy
	// requiring `permissions.added` exits 1. Needs network to fetch the
	// pinned CNPG CRD (see scripts/surface_diff_check.sh).
	surfaceDiffCheck: {
		dependsOn: [build, install]
		command: "sh"
		args: ["scripts/surface_diff_check.sh", "work/surface-diff-check"]
	}
	releaseTest: {command: "cargo", args: ["test", "--locked", "-p", "release"]}
	releaseCheck: {command: "cargo", args: ["clippy", "--locked", "-p", "release", "--all-targets", "--", "-D", "warnings"]}
	coordinationTest: {command: "cargo", args: ["test", "--locked", "-p", "coordination"]}
	inventoryTest: {command: "cargo", args: ["test", "--locked", "-p", "inventory"]}
	compilerCoreTest: {command: "cargo", args: ["test", "--locked", "-p", "compiler-core"]}
	inventoryCheck: {command: "cargo", args: ["clippy", "--locked", "-p", "inventory", "--all-targets", "--", "-D", "warnings"]}
	compilerCoreCheck: {command: "cargo", args: ["clippy", "--locked", "-p", "compiler-core", "--all-targets", "--", "-D", "warnings"]}
	coordinationCheck: {command: "sh", args: ["-c", "cargo clippy --locked -p coordination --all-targets -- -D warnings && cargo check --locked -p coordination --no-default-features"]}
	registryRuntimeCheck: {command: "cargo", args: ["check", "--locked", "-p", "platform", "--no-default-features"]}
	testClusterTest: {command: "cargo", args: ["test", "--locked", "-p", "test-cluster"]}
	testClusterCheck: {command: "cargo", args: ["clippy", "--locked", "-p", "test-cluster", "--all-targets", "--", "-D", "warnings"]}
	// Refreshes scripts/runtime-base from the registry for the digest in
	// scripts/runtime-base/reference; the only task that fetches image metadata.
	runtimeBasePin: {command: "python3", args: ["scripts/runtime_base_pin.py"]}
	// The fixture release and workload fixture image `aps test --cluster` runs.
	// Needs Docker, cargo-cyclonedx and the pinned build images already pulled;
	// see scripts/reactor_fixture.sh. The cluster run also needs kind and the
	// pinned node and proxy images (docs/guides/build-and-deploy.md).
	reactorFixture: {
		dependsOn: [build, install]
		command: "sh"
		args: ["scripts/reactor_fixture.sh", "examples/web-application/program.tsx", "work/reactor"]
	}
	// Builds the fixture itself rather than depending on reactorFixture: a
	// dependency chain two tasks deep makes cuenv's module evaluation time out.
	reactorClusterTest: {
		dependsOn: [build, install]
		command: "sh"
		args: ["-c", "sh scripts/reactor_fixture.sh examples/web-application/program.tsx work/reactor && rm -rf work/reactor/results && target/debug/aps test --cluster --release work/reactor/release --fixture-image work/reactor/fixture-image.tar --output work/reactor/results"]
	}
	// Builds and signs the releases the acceptance suite deploys (the CNPG
	// initial, successor and cleanup examples and the cnpg-owner program),
	// fetches the pinned CNPG operator manifest and writes
	// work/acceptance/inputs.json. Generates the CNPG types itself rather than
	// depending on the types tasks: a dependency chain two tasks deep makes
	// cuenv's module evaluation time out. Needs Docker, cargo-cyclonedx on
	// PATH and the pinned build images local; see docs/reference/ACCEPTANCE.md.
	acceptancePrepare: {
		dependsOn: [build, install]
		command: "sh"
		args: ["-c", "deno run --allow-all packages/compiler/src/crds.ts https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/v1.30.0/config/crd/bases/postgresql.cnpg.io_clusters.yaml Cluster examples/cnpg/.aps/crds/cnpg.ts v1 && deno run --allow-all packages/compiler/src/crds.ts https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/v1.30.0/config/crd/bases/postgresql.cnpg.io_clusters.yaml Cluster examples/cnpg/initial/.aps/crds/cnpg.ts v1 && deno run --allow-all packages/compiler/src/crds.ts https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/v1.30.0/config/crd/bases/postgresql.cnpg.io_clusters.yaml Cluster examples/cnpg/cleanup/.aps/crds/cnpg.ts v1 && deno run --allow-all packages/compiler/src/crds.ts https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/v1.30.0/config/crd/bases/postgresql.cnpg.io_backups.yaml Backup examples/cnpg/cleanup/.aps/crds/backup.ts v1 && cargo run --locked -p acceptance --bin acceptance-prepare -- --output work/acceptance"]
	}
	// Runs every acceptance scenario, each on its own kind cluster, against
	// what acceptancePrepare wrote. Needs Docker, kind and the pinned kind
	// node image local. CI runs the same scenarios split across the four
	// tasks after it.
	acceptanceTest: {
		command: "sh"
		args: ["-c", "APSIDES_ACCEPTANCE_INPUTS=\"$PWD/work/acceptance/inputs.json\" cargo test --locked -p acceptance --test kind -- --ignored --test-threads=1 --nocapture"]
	}
	acceptanceUpgradeTest: {
		command: "sh"
		args: ["-c", "APSIDES_ACCEPTANCE_INPUTS=\"$PWD/work/acceptance/inputs.json\" cargo test --locked -p acceptance --test kind -- --ignored --test-threads=1 --nocapture upgrade:: uncertain:: collision::"]
	}
	acceptancePhasesTest: {
		command: "sh"
		args: ["-c", "APSIDES_ACCEPTANCE_INPUTS=\"$PWD/work/acceptance/inputs.json\" cargo test --locked -p acceptance --test kind -- --ignored --test-threads=1 --nocapture phases::"]
	}
	acceptanceCleanupTest: {
		command: "sh"
		args: ["-c", "APSIDES_ACCEPTANCE_INPUTS=\"$PWD/work/acceptance/inputs.json\" cargo test --locked -p acceptance --test kind -- --ignored --test-threads=1 --nocapture cleanup::"]
	}
	acceptanceCredentialsTest: {
		command: "sh"
		args: ["-c", "APSIDES_ACCEPTANCE_INPUTS=\"$PWD/work/acceptance/inputs.json\" cargo test --locked -p acceptance --test kind -- --ignored --test-threads=1 --nocapture credentials::"]
	}
	// Builds examples/web-application twice with `aps build --cache disabled`
	// and requires identical outputs, file for file (spec §28
	// reproducibility). Needs Docker, cargo-cyclonedx on PATH and the pinned
	// build images local. CI runs it nightly; the compile half runs in the CLI
	// tests on every change.
	reproducibleBuild: {
		dependsOn: [build, install]
		command: "cargo"
		args: ["run", "--locked", "-p", "acceptance", "--bin", "acceptance-reproduce", "--", "--output", "work/reproducible-build"]
	}
	acceptanceCheck: {command: "cargo", args: ["clippy", "--locked", "-p", "acceptance", "--all-targets", "--", "-D", "warnings"]}
	runtimeTest: {command: "cargo", args: ["test", "--locked", "-p", "runtime"]}
	runtimeCheck: {command: "cargo", args: ["clippy", "--locked", "-p", "runtime", "--all-targets", "--", "-D", "warnings"]}
	previewTest: {command: "cargo", args: ["test", "--locked", "-p", "platform", "--features", "deployer", "preview_"]}
	registryTest: {command: "cargo", args: ["test", "--locked", "-p", "deploy", "-p", "platform", "-p", "cli", "--features", "platform/deployer"]}
	registryCheck: {command: "cargo", args: ["clippy", "--locked", "-p", "deploy", "-p", "platform", "-p", "cli", "--features", "platform/deployer", "--all-targets", "--", "-D", "warnings"]}
	authorCompilerTest: {
		dependsOn: [install]
		command: "deno"
		env: {CI: "1"}
		args: ["task", "--filter", "@apsides/compiler", "test"]
	}
	authorCompilerCheck: {
		dependsOn: [install]
		command: "deno"
		env: {CI: "1"}
		args: ["task", "--filter", "@apsides/compiler", "typecheck"]
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
	// aps compile locks the generated controller offline against the workspace
	// Cargo.lock, so every crate for every platform must be fetched, not only
	// those this host builds.
	build: {command: "sh", args: ["-c", "cargo fetch --locked && cargo build --locked -p cli"]}
}
