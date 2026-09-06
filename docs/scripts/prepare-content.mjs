import { mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";

const outputRoot = ".generated-content/docs";

const pages = [
  ["index.md", "index.md"],
  ["getting-started.md", "getting-started.md"],
  ["concepts/architecture.md", "concepts/architecture.md"],
  ["concepts/inventory-prune-upgrades.md", "concepts/inventory-prune-upgrades.md"],
  ["guides/typescript-authoring.md", "guides/typescript-authoring.md"],
  ["guides/build-and-deploy.md", "guides/build-and-deploy.md"],
  ["reference/index.md", "reference/index.md"],
];

const references = [
  ["ROADMAP.md", "roadmap.md", "Rust kernel roadmap"],
  ["m0/BOUNDARY.md", "m0/boundary.md", "Milestone 0 boundary"],
  ["m0/DECISION.md", "m0/decision.md", "Milestone 0 decision"],
  ["m0/LIMITS.md", "m0/limits.md", "Milestone 0 limits"],
  ["v1/CONTROLLER_TSX_V1.md", "v1/controller-tsx-v1.md", "Controller TSX v1"],
  ["reakt/ARTIFACT-RESOLUTION.md", "reakt/artifact-resolution.md", "Optional OCI release retrieval"],
  ["reakt/AUDIT.md", "reakt/audit.md", "Transition audit history"],
  ["reakt/DEPLOYMENT-CONTRACT.md", "reakt/deployment-contract.md", "Provider-neutral deployment protocol"],
  ["reakt/DEPLOYMENT-PERMISSIONS.md", "reakt/deployment-permissions.md", "Deployment permissions"],
  ["reakt/INVENTORY.md", "reakt/inventory.md", "Resource inventory"],
  ["reakt/IR-CONTRACT.md", "reakt/ir-contract.md", "IR contract"],
  ["reakt/LEGACY-POC.md", "reakt/legacy-poc.md", "Legacy OxideLoop POC"],
  ["reakt/OCI-PACKAGING.md", "reakt/oci-packaging.md", "Native controller OCI packaging"],
  ["reakt/PERFORMANCE.md", "reakt/performance.md", "Performance comparison contract"],
  ["reakt/PLATFORM-CONTRACT.md", "reakt/platform-contract.md", "Platform contract"],
  ["reakt/SPEC-v0.2.md", "reakt/spec-v0-2.md", "Reakt specification 0.2"],
  ["reakt/STATE-MIGRATION.md", "reakt/state-migration.md", "State migration contract"],
  ["reakt/STATUS.md", "reakt/status.md", "Reakt implementation status"],
  ["reakt/TOOLCHAIN.md", "reakt/toolchain.md", "Toolchain choices"],
];

function withFrontmatter(source, title) {
  const withoutHeading = source
    .replace(/^# .+\n+/, "")
    .replace(
      /\]\(AUDIT\.md(#[^)]+)?\)/g,
      (_, fragment = "") => `](../audit/${fragment})`,
    )
    .replace(
      /\]\(STATE-MIGRATION\.md(#[^)]+)?\)/g,
      (_, fragment = "") => `](../state-migration/${fragment})`,
    )
    .replace(
      /\]\(\.\.\/\.\.\/packages\/example-bundle\/src\/controller\.tsx\)/g,
      "](https://github.com/rawkode/apsides/blob/main/packages/example-bundle/src/controller.tsx)",
    )
    .replace(
      /\]\(\.\.\/\.\.\/examples\/web-application\/README\.md\)/g,
      "](https://github.com/rawkode/apsides/blob/main/examples/web-application/README.md)",
    );
  const escapedTitle = JSON.stringify(title);
  return `---\ntitle: ${escapedTitle}\n---\n\n${withoutHeading}`;
}

async function copy(sourcePath, targetPath, title) {
  const source = await readFile(sourcePath, "utf8");
  const target = join(outputRoot, targetPath);
  await mkdir(dirname(target), { recursive: true });
  await writeFile(target, title ? withFrontmatter(source, title) : source);
}

await rm(".generated-content", { recursive: true, force: true });
await Promise.all([
  ...pages.map(([source, target]) => copy(source, target)),
  ...references.map(([source, target, title]) => copy(source, target, title)),
]);
