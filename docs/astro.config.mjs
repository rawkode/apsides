import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";

export default defineConfig({
  integrations: [
    starlight({
      title: "Apsides",
      description: "Documentation for the Apsides compiled Kubernetes controller toolchain",
      customCss: ["./src/styles/custom.css"],
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
            { label: "Overview", slug: "" },
            { label: "Getting started", slug: "getting-started" },
            { label: "Implementation status", slug: "reakt/status" },
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
          ],
        },
        {
          label: "Reference",
          items: [
            { label: "Reference map", slug: "reference" },
            { label: "Specification 0.2", slug: "reakt/spec-v0-2" },
            { label: "IR contract", slug: "reakt/ir-contract" },
            { label: "Platform contract", slug: "reakt/platform-contract" },
            { label: "Deployment protocol", slug: "reakt/deployment-contract" },
            { label: "Deployment permissions", slug: "reakt/deployment-permissions" },
            { label: "Deployment review", slug: "reakt/review-contract" },
            { label: "Artifact resolution", slug: "reakt/artifact-resolution" },
            { label: "OCI packaging", slug: "reakt/oci-packaging" },
            { label: "Resource inventory", slug: "reakt/inventory" },
            { label: "Transition audit history", slug: "reakt/audit" },
            { label: "State capacity", slug: "reakt/state-capacity" },
            { label: "Toolchain", slug: "reakt/toolchain" },
            { label: "Performance contract", slug: "reakt/performance" },
          ],
        },
      ],
    }),
  ],
});
