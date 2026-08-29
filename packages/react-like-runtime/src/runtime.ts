import type { RenderOutput, ResourceNode } from "@oxideloop/sdk";
import { currentDependencies, resetDependencies } from "@oxideloop/sdk/hooks";
import { ExplicitEmpty, type Element } from "./jsx-runtime.js";
export interface RuntimeRenderer { render(element: Element): RenderOutput; }
export function createRenderer(): RuntimeRenderer {
  return { render(element) {
    resetDependencies();
    if (element?.type === ExplicitEmpty) {
      return { disposition: "explicitEmpty", resources: [], dependencies: [] };
    }
    const resources: ResourceNode[] = [];
    walk(element, resources);
    if (resources.length === 0) throw new RenderTreeError("an empty graph requires explicitEmpty()");
    return { disposition: "apply", resources, dependencies: currentDependencies() };
  } };
}
function walk(node: unknown, resources: ResourceNode[]): void {
  if (node == null || typeof node === "boolean") throw new RenderTreeError("null and boolean render nodes are unsupported");
  if (Array.isArray(node)) { for (const child of node) walk(child, resources); return; }
  if (typeof node !== "object") throw new RenderTreeError("primitive render nodes are unsupported");
  if ("apiVersion" in node && "kind" in node && "metadata" in node) {
    assertResourceNode(node);
    resources.push(node);
    return;
  }
  const element = node as Element;
  if (element.type === Symbol.for("oxideloop.fragment")) { walk(element.props.children, resources); return; }
  if (element.type === ExplicitEmpty) throw new RenderTreeError("explicitEmpty() must be the root render value");
  if (typeof element.type === "function") { walk((element.type as (p: Record<string, unknown>) => unknown)(element.props), resources); return; }
  if (typeof element.type === "string") {
    const { children: _children, ...props } = element.props;
    assertResourceNode(props);
    resources.push(props);
    return;
  }
  throw new RenderTreeError("unrecognized render element");
}

function assertResourceNode(node: object): asserts node is ResourceNode {
  const candidate = node as Partial<ResourceNode>;
  if (typeof candidate.apiVersion !== "string" || candidate.apiVersion.length === 0 ||
      typeof candidate.kind !== "string" || candidate.kind.length === 0 ||
      candidate.metadata === null || typeof candidate.metadata !== "object" ||
      typeof candidate.metadata.name !== "string" || candidate.metadata.name.length === 0) {
    throw new RenderTreeError("malformed Kubernetes resource node");
  }
}

export class RenderTreeError extends Error {}
