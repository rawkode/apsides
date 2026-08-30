import type { JsonValue, RenderInputV0, RenderOutputV0 } from "@oxideloop/sdk";
import { createHookSession } from "@oxideloop/sdk/hooks";
import { createRenderer } from "@oxideloop/react-like-runtime";
import { jsx } from "@oxideloop/react-like-runtime/jsx-runtime";
import { Workload } from "./workload.js";
import { hookEnvironment, renderOutput } from "./host-environment.js";

export function render(input: RenderInputV0): RenderOutputV0 {
  const session = createHookSession(hookEnvironment(input));
  const rendered = createRenderer(session).render(jsx(Workload, { name: input.root.name, namespace: input.root.namespace, session }));
  return renderOutput(
    rendered.resources as unknown as JsonValue[],
    rendered.dependencies,
    [],
    rendered.disposition,
  );
}
