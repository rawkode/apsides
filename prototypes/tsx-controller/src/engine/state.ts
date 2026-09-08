import { readFileSync } from "node:fs";
import { join } from "node:path";
import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import { InMemoryService } from "alchemy/State/InMemoryState";
import { State, StateStoreError, type StateService } from "alchemy/State/State";
import { encodeState, reviveState } from "alchemy/State/StateEncoding";
import { atomicJSON } from "./world";

/** Adds durable snapshots to Alchemy's own state service; single writer only. */
export function durableState(directory: string, beforeSave?: (value: unknown) => void) {
  return Layer.effect(State, Effect.sync(() => {
    const file = join(directory, "alchemy-state.json");
    let data: { resources: Parameters<typeof InMemoryService>[0]; outputs: Parameters<typeof InMemoryService>[1] };
    try { data = JSON.parse(readFileSync(file, "utf8"), reviveState); }
    catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
      data = { resources: {}, outputs: {} };
    }
    return InMemoryService(data.resources, data.outputs).pipe(Effect.map((base) => {
      const save = <A>(operation: Effect.Effect<A, StateStoreError>, value?: unknown) =>
        Effect.try({ try: () => beforeSave?.(value), catch: (cause) => new StateStoreError({ message: String(cause) }) }).pipe(
          Effect.andThen(operation),
          Effect.tap(() => Effect.try({
            try: () => atomicJSON(file, encodeState(data)),
            catch: (cause) => new StateStoreError({ message: String(cause) }),
          })),
        );
      return {
        ...base,
        id: "prototype-local-snapshot",
        set: (request) => save(base.set(request), request.value),
        delete: (request) => save(base.delete(request)),
        deleteStack: (request) => save(base.deleteStack(request)),
        setOutput: (request) => save(base.setOutput(request)),
      } satisfies StateService;
    }));
  }));
}
