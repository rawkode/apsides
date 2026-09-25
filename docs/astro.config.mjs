import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";

export default defineConfig({
  site: "https://apsides.dev",
  integrations: [
    starlight({
      title: "Apsides",
      description: "Documentation for the Apsides compiled Kubernetes controller toolchain",
      customCss: ["./src/styles/custom.css"],
      components: {
        ThemeProvider: "./src/components/ThemeProvider.astro",
        ThemeSelect: "./src/components/ThemeSelect.astro",
      },
      social: [
        {
          icon: "github",
          label: "Apsides on GitHub",
          href: "https://github.com/rawkode/apsides",
        },
      ],
      lastUpdated: true,
      tableOfContents: { minHeadingLevel: 2, maxHeadingLevel: 3 },
      sidebar: [
        {
          label: "Start here",
          items: [
            { label: "Overview", slug: "index" },
            { label: "Getting started", slug: "getting-started" },
            { label: "Implementation status", slug: "reference/status" },
          ],
        },
        {
          label: "Concepts",
          items: [
            { label: "Architecture", slug: "concepts/architecture" },
            {
              label: "Inventory, prune and upgrades",
              slug: "concepts/inventory-prune-upgrades",
            },
          ],
        },
        {
          label: "Guides",
          items: [
            { label: "TypeScript authoring", slug: "guides/typescript-authoring" },
            { label: "Build and deploy", slug: "guides/build-and-deploy" },
            { label: "Custom resources", slug: "guides/custom-resources" },
            { label: "Event handlers", slug: "guides/event-handlers" },
            { label: "Surface diff in CI", slug: "guides/surface-diff" },
          ],
        },
        {
          label: "Reference",
          items: [
            { label: "Reference map", slug: "reference" },
            { label: "Specification", slug: "reference/spec" },
            { label: "Acceptance suite", slug: "reference/acceptance" },
            { label: "IR contract", slug: "reference/ir-contract" },
            { label: "Platform contract", slug: "reference/platform-contract" },
            { label: "Deployment protocol", slug: "reference/deployment-contract" },
            { label: "Deployment permissions", slug: "reference/deployment-permissions" },
            { label: "Deployment review", slug: "reference/review-contract" },
            { label: "Artifact resolution", slug: "reference/artifact-resolution" },
            { label: "OCI packaging", slug: "reference/oci-packaging" },
            { label: "Resource inventory", slug: "reference/inventory" },
            { label: "Transition audit history", slug: "reference/audit" },
            { label: "State capacity", slug: "reference/state-capacity" },
            { label: "CLI distribution", slug: "reference/cli-distribution" },
            { label: "Toolchain", slug: "reference/toolchain" },
          ],
        },
      ],
    }),
  ],
});
