# Reakt TypeScript example

Run the complete authoring example with Deno and access only to the source file whose digest it records:

```sh
deno run --allow-read=examples/reakt/program.ts examples/reakt/domain.ts --production
```

The default and `--production` write raw production IR. The small Deno wrapper hashes `program.ts` and passes that digest into the author module; the author module itself performs no filesystem or network I/O. `--fixture` writes raw fixture-substituted IR for the compiler's explicitly test-only path. `--fixture-bundle` writes the companion artifact that records synthetic observed ConfigMap and Secret values and the workload overlay as `testOnly: true` and `productionReady: false`. The overlay replaces every workload container with the locally loaded `reakt-fixture:local` image and `imagePullPolicy: Never`; neither it nor the synthetic input values enter production IR.

The supported source-build path uses the checked-in canonical lock:

```sh
scripts/reakt/source-build.sh examples/reakt/program.ts work/reakt-source
```

After changing source, refresh the explicit closure with `scripts/reakt/source-lock.sh examples/reakt/program.ts`. List every additional relative source module as another argument. The build performs a Deno type check, two independent Oxc/Rolldown builds, and two fresh denied-permission Deno evaluations in a pinned SES Compartment. It emits production and fixture IR, a real bundle source map, declaration source mapping, the build-input/toolchain receipt, a compiler-reconstructed overlay diff, the fixture catalogue, and source-linked coverage requirements. Output must be a fresh directory.
