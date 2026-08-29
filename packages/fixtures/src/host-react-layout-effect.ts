import React from "react";
import type { RenderInputV0, RenderOutputV0 } from "@oxideloop/sdk";
import { createOxideLoopRoot } from "@oxideloop/react-renderer";
import { renderOutput } from "./host-environment.js";

let callbacks = 0;

function LayoutEffectProbe() {
  React.useLayoutEffect(() => {
    callbacks += 1;
  }, []);
  return React.createElement("ConfigMap", {
    apiVersion: "v1",
    kind: "ConfigMap",
    metadata: { name: "layout-effect-probe" },
  });
}

export function render(_input: RenderInputV0): RenderOutputV0 {
  callbacks = 0;
  const root = createOxideLoopRoot();
  root.render(React.createElement(LayoutEffectProbe));
  return renderOutput([], [], callbacks === 0 ? [] : [{
    severity: "error",
    code: "actual-react-layout-effect-executed",
    message: `count=${callbacks}`,
  }], "hold");
}
