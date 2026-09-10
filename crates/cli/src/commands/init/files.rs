//! Contents written into a fresh authoring project.
//!
//! Project templates are real `.tsx` and `.json` files under `templates/` so
//! they stay readable and formattable as the languages they are. They sit
//! inside `src/` because the distribution source closure is exactly the tracked
//! `crates/*/src` tree. The SDK is embedded from the canonical authoring
//! package; init never copies it out of an installed tree.

pub(super) const PROJECT_FILES: &[(&str, &str)] = &[
    ("program.tsx", include_str!("templates/program.tsx")),
    ("deno.json", include_str!("templates/deno.json")),
    ("tsconfig.json", include_str!("templates/tsconfig.json")),
    (".gitignore", ".aps/\n"),
];

pub(super) const SDK_FILES: &[(&str, &str)] = &[
    (
        "index.ts",
        include_str!("../../../../../packages/kubernetes/src/index.ts"),
    ),
    (
        "model.ts",
        include_str!("../../../../../packages/kubernetes/src/model.ts"),
    ),
    (
        "runtime.ts",
        include_str!("../../../../../packages/kubernetes/src/runtime.ts"),
    ),
    (
        "compiler.ts",
        include_str!("../../../../../packages/kubernetes/src/compiler.ts"),
    ),
    (
        "jsx-runtime.ts",
        include_str!("../../../../../packages/kubernetes/src/jsx-runtime.ts"),
    ),
    (
        "jsx-dev-runtime.ts",
        include_str!("../../../../../packages/kubernetes/src/jsx-dev-runtime.ts"),
    ),
];
