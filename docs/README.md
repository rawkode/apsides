# Apsides documentation site

This directory is a self-contained [Astro Starlight](https://starlight.astro.build/) site. The
existing files under `reakt/` are the detailed implementation and contract
references; the other pages provide a guided path through them.

Requires Node.js 22.12 or newer.

```sh
cd docs
npm ci
npm run dev
```

Build the static site with:

```sh
cd docs
npm ci
npm run build
```

`npm run build` checks content schemas, writes the static site to `docs/dist/`,
then validates every internal HTML target and fragment. Broken internal links
fail the build.

`scripts/prepare-content.mjs` creates an ignored Starlight content tree before
each development or production build. It mirrors the canonical contract files
without modifying them and supplies the frontmatter Starlight requires.
