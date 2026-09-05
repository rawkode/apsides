package cuenv

import "github.com/cuenv/cuenv/schema"

schema.#Project & {name: "apsides"}

tasks: {
	generate: [
		{command: "cargo", args: ["run", "--locked", "-p", "controller-api", "--bin", "generate-controller-api"]},
		{command: "cargo", args: ["run", "--locked", "-p", "renderer-abi", "--bin", "generate", "--", "."]},
		{command: "cargo", args: ["run", "--locked", "-p", "renderer-protocol", "--bin", "generate-renderer-protocol"]},
		{command: "cargo", args: ["run", "--locked", "-p", "bundle-contract", "--bin", "generate-bundle-contract", "--", "."]},
		{command: "cargo", args: ["run", "--locked", "-p", "mutation-model", "--bin", "generate-probe-schema", "--", "."]},
	]
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
