# Initial database deployment

This is the first version of the [CNPG example](../README.md). It creates the
CloudNativePG `Cluster` and the API configuration ConfigMap. The parent example
retains both resources and adds PostgREST, its Service and the observed inputs.
The upgrade therefore exercises an expanded permission ceiling without replacing
the database.

From the repository root:

```sh
cuenv task cnpgInitialTypes
cuenv exec -- target/debug/aps compile \
  --source examples/cnpg/initial/program.tsx \
  --output work/cnpg-initial-compiled
```

Use a fresh output directory. The generated types stay under ignored `.aps/`.
After editing the source or regenerating different types, refresh the lock:

```sh
cuenv exec -- target/debug/aps lock \
  --source examples/cnpg/initial/program.tsx \
  --modules examples/cnpg/initial/.aps/crds/cnpg.ts
```

Both versions use the same domain and namespace. They require a separately
installed CloudNativePG operator and explicit policy acceptance of
`opaque-custom-resource-effects`. Compilation does not establish successful
installation, database readiness or an upgrade.
