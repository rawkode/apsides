import type { JsonValue, RenderInputV0, RenderOutputV0 } from "@oxideloop/sdk";
import { createRenderer, explicitEmpty } from "@oxideloop/react-like-runtime";
import { renderOutput } from "./host-environment.js";

export function render(_input: RenderInputV0): RenderOutputV0 {
  const rendered = createRenderer().render(explicitEmpty());
  return renderOutput(
    rendered.resources as unknown as JsonValue[],
    rendered.dependencies,
    [],
    rendered.disposition,
  );
}
