import ReactReconciler from "react-reconciler";
import type { ReactNode } from "react";
import { currentDependencies, resetDependencies } from "@oxideloop/sdk/hooks";
export interface NeutralNode { readonly type: string; props: Record<string, unknown>; readonly children: NeutralNode[]; }
export interface RendererTrace { readonly events: readonly string[]; readonly dependencies: ReturnType<typeof currentDependencies>; }
type Container = { children: NeutralNode[]; events: string[] };
const hostConfig: any = {
  now: () => 0, supportsMutation: true, supportsPersistence: false, supportsHydration: false,
  resolveUpdatePriority: () => 2, setCurrentUpdatePriority: () => {}, getCurrentUpdatePriority: () => 2,
  getCurrentEventPriority: () => 2, scheduleTimeout: (..._args: unknown[]) => 0, cancelTimeout: (_id: unknown) => {}, noTimeout: -1,
  supportsMicrotasks: false, scheduleMicrotask: () => { throw new Error("React microtasks are unsupported"); },
  maySuspendCommit: () => false, preloadInstance: () => true, startSuspendingCommit: () => {}, suspendInstance: () => {}, waitForCommitToBeReady: () => null,
  isPrimaryRenderer: false, getRootHostContext: () => ({}), getChildHostContext: () => ({}),
  prepareForCommit: () => null, resetAfterCommit: (c: Container) => c.events.push("commit"), shouldSetTextContent: () => false,
  createInstance: (type: string, props: Record<string, unknown>) => ({ type, props, children: [] }),
  createTextInstance: (text: string) => ({ type: "#text", props: { text }, children: [] }),
  appendInitialChild: (p: NeutralNode, c: NeutralNode) => p.children.push(c), appendChild: (p: NeutralNode, c: NeutralNode) => p.children.push(c),
  appendChildToContainer: (c: Container, n: NeutralNode) => c.children.push(n), insertBefore: (p: NeutralNode, c: NeutralNode, b: NeutralNode) => p.children.splice(p.children.indexOf(b), 0, c),
  insertInContainerBefore: (c: Container, n: NeutralNode, b: NeutralNode) => c.children.splice(c.children.indexOf(b), 0, n),
  removeChild: (p: NeutralNode, c: NeutralNode) => p.children.splice(p.children.indexOf(c), 1), removeChildFromContainer: (p: Container, c: NeutralNode) => p.children.splice(p.children.indexOf(c), 1),
  clearContainer: (c: Container) => { c.children.length = 0; }, finalizeInitialChildren: () => false, prepareUpdate: () => true,
  commitUpdate: (n: NeutralNode, _t: string, _old: unknown, next: Record<string, unknown>) => { n.props = next; }, commitTextUpdate: (n: NeutralNode, _o: string, text: string) => { n.props.text = text; },
  getPublicInstance: (n: NeutralNode) => n, detachDeletedInstance: () => {}, prepareScopeUpdate: () => {}, getInstanceFromNode: () => null,
};
const reconciler = ReactReconciler(hostConfig);
export function createOxideLoopRoot(): { render(node: ReactNode): void; snapshot(): NeutralNode[]; trace(): RendererTrace } {
  const container: Container = { children: [], events: [] };
  const root = reconciler.createContainer(container, 0, null, false, null, "oxideloop", (error: unknown) => { throw error; }, null);
  return { render(node) { resetDependencies(); (reconciler as any).flushSyncFromReconciler(() => reconciler.updateContainer(node, root, null, () => container.events.push("render"))); }, snapshot: () => container.children, trace: () => ({ events: container.events, dependencies: currentDependencies() }) };
}
