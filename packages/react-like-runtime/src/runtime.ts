import type { RenderGraph, ResourceNode } from "@oxideloop/sdk";
import type { HookSession } from "@oxideloop/sdk/hooks";
import { ExplicitEmpty, type Element } from "./jsx-runtime.js";
export interface RuntimeRenderer { render(element: Element): RenderGraph; }
export function createRenderer(session?: HookSession): RuntimeRenderer {
  return { render(element) {
    if (element?.type === ExplicitEmpty) {
      assertNoChildren(element.props, "explicitEmpty()");
      return { disposition: "explicitEmpty", resources: [], dependencies: [], automations: [] };
    }
    const resources: ResourceNode[] = [];
    walk(element, resources);
    if (resources.length === 0) throw new RenderTreeError("an empty graph requires explicitEmpty()");
    return {
      disposition: "apply",
      resources,
      dependencies: session?.dependencies ?? [],
      automations: session?.automations ?? [],
    };
  } };
}
function walk(node: unknown, resources: ResourceNode[]): void {
  if (node == null || typeof node === "boolean") throw new RenderTreeError("null and boolean render nodes are unsupported");
  if (Array.isArray(node)) { for (const child of node) walk(child, resources); return; }
  if (typeof node !== "object") throw new RenderTreeError("primitive render nodes are unsupported");
  if ("apiVersion" in node && "kind" in node && "metadata" in node) {
    if (Object.hasOwn(node, "children")) throw new RenderTreeError("resource nodes cannot have children");
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
    assertNoChildren(element.props, "intrinsic resource");
    assertResourceNode(props);
    resources.push(props);
    return;
  }
  throw new RenderTreeError("unrecognized render element");
}

function assertNoChildren(props: Record<string, unknown>, owner: string): void {
  if (Object.hasOwn(props, "children")) {
    throw new RenderTreeError(`${owner} cannot have children`);
  }
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
