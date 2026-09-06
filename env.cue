package cuenv

import "github.com/cuenv/cuenv/schema"

schema.#Project & {name: "apsides"}

tasks: {
	coordinationTest: {command: "cargo", args: ["test", "--locked", "-p", "coordination"]}
	coordinationCheck: [
		{command: "cargo", args: ["clippy", "--locked", "-p", "coordination", "--all-targets", "--", "-D", "warnings"]},
		{command: "cargo", args: ["check", "--locked", "-p", "coordination", "--no-default-features"]},
	]
	registryRuntimeCheck: {command: "cargo", args: ["check", "--locked", "-p", "platform", "--no-default-features"]}
	registryTest: {command: "cargo", args: ["test", "--locked", "-p", "deploy", "-p", "platform", "-p", "cli", "--features", "platform/test-support"]}
	registryCheck: {command: "cargo", args: ["clippy", "--locked", "-p", "deploy", "-p", "platform", "-p", "cli", "--features", "platform/test-support", "--all-targets", "--", "-D", "warnings"]}
	generate: [
		{command: "cargo", args: ["run", "--locked", "-p", "controller-api", "--bin", "generate-controller-api"]},
		{command: "cargo", args: ["run", "--locked", "-p", "renderer-abi", "--bin", "generate", "--", "."]},
		{command: "cargo", args: ["run", "--locked", "-p", "renderer-protocol", "--bin", "generate-renderer-protocol"]},
		{command: "cargo", args: ["run", "--locked", "-p", "bundle-contract", "--bin", "generate-bundle-contract", "--", "."]},
		{command: "cargo", args: ["run", "--locked", "-p", "mutation-model", "--bin", "generate-probe-schema", "--", "."]},
	]
	authorCompilerTest: {
		dependsOn: [install]
		command: "pnpm"
		env: {CI: "1"}
		args: ["--filter", "@oxideloop/bundle-builder", "run", "test:reakt"]
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
	"sdk-check": {dependsOn: [install, generate], command: "pnpm", args: ["--filter", "@oxideloop/sdk", "typecheck"]}
	"legacy-example": {dependsOn: [install, generate], command: "make", args: ["example"]}
	build: {dependsOn: [generate], command: "cargo", args: ["build", "--locked", "-p", "cli"]}
	"test-generated": {
		dependsOn: [generate]
		command: "cargo"
		args: ["test", "--locked", "-p", "renderer-abi", "-p", "renderer-protocol", "-p", "bundle-contract", "-p", "mutation-model", "-p", "controller-api"]
	}
}
