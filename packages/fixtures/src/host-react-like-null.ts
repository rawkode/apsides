import type { JsonValue, RenderInputV0, RenderOutputV0 } from "@oxideloop/sdk";
import { createRenderer } from "@oxideloop/react-like-runtime";
import { jsx } from "@oxideloop/react-like-runtime/jsx-runtime";
import { renderOutput } from "./host-environment.js";

function NullComponent() {
  return null;
}

export function render(_input: RenderInputV0): RenderOutputV0 {
  const rendered = createRenderer().render(jsx(NullComponent, {}));
  return renderOutput(
    rendered.resources as unknown as JsonValue[],
    rendered.dependencies,
    [],
    rendered.disposition,
  );
}
