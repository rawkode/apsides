# Repository guidance

- Never commit generated code, schemas, rendered documentation, or build outputs. Commit their source definitions and generators instead. Dependency lockfiles remain tracked for reproducibility.
- Keep only the current implementation. Remove superseded code, compatibility fallbacks, obsolete tasks and historical design documentation when replacing them. Use Git history for past implementations.
- Use `cuenv task docsDev` and `cuenv task docsBuild` for the Astro site. Both prepare the ignored content mirror from canonical Markdown; never commit that mirror or `docs/dist`.
- Add new generation steps to cuenv codegen or tasks, with build/test dependencies that work from a fresh checkout.
- Keep internal crates named by responsibility without a product-name prefix.
