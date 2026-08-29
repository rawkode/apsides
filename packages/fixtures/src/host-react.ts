import React from "react";
import type { JsonValue, RenderInputV0, RenderOutputV0 } from "@oxideloop/sdk";
import { withHookEnvironment } from "@oxideloop/sdk/hooks";
import { createOxideLoopRoot } from "@oxideloop/react-renderer";
import { ReactWorkload } from "./workload.js";
import { hookEnvironment, renderOutput } from "./host-environment.js";

export function render(input: RenderInputV0): RenderOutputV0 {
  const root = createOxideLoopRoot();
  withHookEnvironment(hookEnvironment(input), () => {
    root.render(React.createElement(ReactWorkload, { name: input.root.name, namespace: input.root.namespace }));
  });
  const resources = root.snapshot().map((node) => {
    const { children: _children, ...props } = node.props;
    return {
      apiVersion: String(props.apiVersion ?? ""),
      kind: String(props.kind ?? node.type),
      metadata: props.metadata,
      ...(props.spec === undefined ? {} : { spec: props.spec }),
    } as JsonValue;
  });
  return renderOutput(resources, root.trace().dependencies, [], resources.length === 0 ? "hold" : "apply");
}
