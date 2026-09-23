# Apsides documentation site

This directory is a self-contained [Astro Starlight](https://starlight.astro.build/) site. The
existing files under `reference/` are the detailed implementation and contract
references; the other pages provide a guided path through them.

Requires cuenv 0.56.1 and Deno 2.9.5 or newer. Run these tasks from the repository root.

```sh
cuenv task docsDev
```

Build the static site with:

```sh
cuenv task docsBuild
```

`cuenv task docsBuild` checks content schemas, writes the static site to `docs/dist/`,
then validates every internal HTML target and fragment. Broken internal links
fail the build.

`scripts/prepare-content.mjs` creates an ignored Starlight content tree before
each development or production build. It mirrors the canonical contract files
without modifying them and supplies the frontmatter Starlight requires.

This directory is deliberately excluded from the root Deno workspace so the
Astro toolchain stays out of the compiler closure. Deno prints a "not a member
of the workspace" warning when running tasks here; that warning is expected.
