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
  ["reference/ARTIFACT-RESOLUTION.md", "reference/artifact-resolution.md", "Optional OCI release retrieval"],
  ["reference/AUDIT.md", "reference/audit.md", "Transition audit history"],
  ["reference/CLI-DISTRIBUTION.md", "reference/cli-distribution.md", "CLI distribution"],
  ["reference/DEPLOYMENT-CONTRACT.md", "reference/deployment-contract.md", "Provider-neutral deployment protocol"],
  ["reference/DEPLOYMENT-PERMISSIONS.md", "reference/deployment-permissions.md", "Deployment permissions"],
  ["reference/INVENTORY.md", "reference/inventory.md", "Resource inventory"],
  ["reference/IR-CONTRACT.md", "reference/ir-contract.md", "IR contract"],
  ["reference/OCI-PACKAGING.md", "reference/oci-packaging.md", "Native controller OCI packaging"],
  ["reference/PERFORMANCE.md", "reference/performance.md", "Performance comparison contract"],
  ["reference/PRODUCTION-EVIDENCE.md", "reference/production-evidence.md", "Production acceptance evidence"],
  ["reference/PLATFORM-CONTRACT.md", "reference/platform-contract.md", "Platform contract"],
  ["reference/REVIEW-CONTRACT.md", "reference/review-contract.md", "Deployment review contract"],
  ["reference/SPEC-v0.2.md", "reference/spec-v0-2.md", "Apsides specification 0.2"],
  ["reference/STATE-CAPACITY.md", "reference/state-capacity.md", "State capacity admission"],
  ["reference/STATUS.md", "reference/status.md", "Apsides implementation status"],
  ["reference/TOOLCHAIN.md", "reference/toolchain.md", "Toolchain choices"],
];

const withFrontmatter = (source, title) => {
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
};

const copy = async (sourcePath, targetPath, title) => {
  const source = await readFile(sourcePath, "utf8");
  const target = join(outputRoot, targetPath);
  await mkdir(dirname(target), { recursive: true });
  await writeFile(target, title ? withFrontmatter(source, title) : source);
};

await rm(".generated-content", { recursive: true, force: true });
await Promise.all([
  ...pages.map(([source, target]) => copy(source, target)),
  ...references.map(([source, target, title]) => copy(source, target, title)),
]);
