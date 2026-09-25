import { mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";

const outputRoot = ".generated-content/docs";

const pages = [
  ["index.md", "index.md"],
  ["404.md", "404.md"],
  ["getting-started.md", "getting-started.md"],
  ["concepts/architecture.md", "concepts/architecture.md"],
  ["concepts/inventory-prune-upgrades.md", "concepts/inventory-prune-upgrades.md"],
  ["guides/typescript-authoring.md", "guides/typescript-authoring.md"],
  ["guides/build-and-deploy.md", "guides/build-and-deploy.md"],
  ["guides/custom-resources.md", "guides/custom-resources.md"],
  ["guides/surface-diff.md", "guides/surface-diff.md"],
  ["guides/event-handlers.md", "guides/event-handlers.md"],
  ["reference/index.md", "reference/index.md"],
];

const references = [
  ["reference/ACCEPTANCE.md", "reference/acceptance.md", "Acceptance suite"],
  ["reference/ARTIFACT-RESOLUTION.md", "reference/artifact-resolution.md", "Optional OCI release retrieval"],
  ["reference/AUDIT.md", "reference/audit.md", "Transition audit history"],
  ["reference/CLI-DISTRIBUTION.md", "reference/cli-distribution.md", "CLI distribution"],
  ["reference/DEPLOYMENT-CONTRACT.md", "reference/deployment-contract.md", "Provider-neutral deployment protocol"],
  ["reference/DEPLOYMENT-PERMISSIONS.md", "reference/deployment-permissions.md", "Deployment permissions"],
  ["reference/INVENTORY.md", "reference/inventory.md", "Resource inventory"],
  ["reference/IR-CONTRACT.md", "reference/ir-contract.md", "IR contract"],
  ["reference/OCI-PACKAGING.md", "reference/oci-packaging.md", "Native controller OCI packaging"],
  ["reference/PLATFORM-CONTRACT.md", "reference/platform-contract.md", "Platform contract"],
  ["reference/REVIEW-CONTRACT.md", "reference/review-contract.md", "Deployment review contract"],
  ["reference/SPEC.md", "reference/spec.md", "Apsides specification"],
  ["reference/STATE-CAPACITY.md", "reference/state-capacity.md", "State capacity admission"],
  ["reference/STATUS.md", "reference/status.md", "Apsides implementation status"],
  ["reference/TOOLCHAIN.md", "reference/toolchain.md", "Toolchain choices"],
];

const withFrontmatter = (source, title) => {
  const withoutHeading = source
    .replace(/^# .+\n+/, "")
    .replace(
      /\]\(SPEC\.md(#[^)]+)?\)/g,
      (_, fragment = "") => `](../spec/${fragment})`,
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
      /\]\(OCI-PACKAGING\.md(#[^)]+)?\)/g,
      (_, fragment = "") => `](../oci-packaging/${fragment})`,
    )
    .replace(
      /\]\(\.\.\/\.\.\/examples\/cnpg\/cleanup\/README\.md\)/g,
      "](https://github.com/rawkode/apsides/blob/main/examples/cnpg/cleanup/README.md)",
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
