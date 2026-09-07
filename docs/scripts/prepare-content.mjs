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
  ["guides/custom-resources.md", "guides/custom-resources.md"],
  ["reference/index.md", "reference/index.md"],
];

const references = [
  ["reakt/ARTIFACT-RESOLUTION.md", "reakt/artifact-resolution.md", "Optional OCI release retrieval"],
  ["reakt/AUDIT.md", "reakt/audit.md", "Transition audit history"],
  ["reakt/CLI-DISTRIBUTION.md", "reakt/cli-distribution.md", "CLI distribution"],
  ["reakt/DEPLOYMENT-CONTRACT.md", "reakt/deployment-contract.md", "Provider-neutral deployment protocol"],
  ["reakt/DEPLOYMENT-PERMISSIONS.md", "reakt/deployment-permissions.md", "Deployment permissions"],
  ["reakt/INVENTORY.md", "reakt/inventory.md", "Resource inventory"],
  ["reakt/IR-CONTRACT.md", "reakt/ir-contract.md", "IR contract"],
  ["reakt/OCI-PACKAGING.md", "reakt/oci-packaging.md", "Native controller OCI packaging"],
  ["reakt/PERFORMANCE.md", "reakt/performance.md", "Performance comparison contract"],
  ["reakt/PRODUCTION-EVIDENCE.md", "reakt/production-evidence.md", "Production acceptance evidence"],
  ["reakt/PLATFORM-CONTRACT.md", "reakt/platform-contract.md", "Platform contract"],
  ["reakt/REVIEW-CONTRACT.md", "reakt/review-contract.md", "Deployment review contract"],
  ["reakt/SPEC-v0.2.md", "reakt/spec-v0-2.md", "Apsides specification 0.2"],
  ["reakt/STATE-CAPACITY.md", "reakt/state-capacity.md", "State capacity admission"],
  ["reakt/STATUS.md", "reakt/status.md", "Apsides implementation status"],
  ["reakt/TOOLCHAIN.md", "reakt/toolchain.md", "Toolchain choices"],
];

function withFrontmatter(source, title) {
  const withoutHeading = source
    .replace(/^# .+\n+/, "")
    .replace(
      /\]\(REVIEW-CONTRACT\.md(#[^)]+)?\)/g,
      (_, fragment = "") => `](../review-contract/${fragment})`,
    )
    .replace(
      /\]\(STATE-CAPACITY\.md(#[^)]+)?\)/g,
      (_, fragment = "") => `](../state-capacity/${fragment})`,
    )
    .replace(
      /\]\(SPEC-v0\.2\.md(#[^)]+)?\)/g,
      (_, fragment = "") => `](../spec-v0-2/${fragment})`,
    )
    .replace(
      /\]\(AUDIT\.md(#[^)]+)?\)/g,
      (_, fragment = "") => `](../audit/${fragment})`,
    )
    .replace(
      /\]\(CLI-DISTRIBUTION\.md(#[^)]+)?\)/g,
      (_, fragment = "") => `](../cli-distribution/${fragment})`,
    )
    .replace(
      /\]\(STATUS\.md(#[^)]+)?\)/g,
      (_, fragment = "") => `](../status/${fragment})`,
    )
    .replace(
      /\]\(\.\.\/\.\.\/examples\/cnpg\/cleanup\/README\.md\)/g,
      "](https://github.com/rawkode/apsides/blob/main/examples/cnpg/cleanup/README.md)",
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
