import { makeElement, type ReaktElement, type ReaktNode } from "./runtime.ts";
export { Fragment } from "./runtime.ts";
export function jsx(type: unknown, props: unknown, _key?: unknown): ReaktElement { return makeElement(type, props); }
export const jsxs = jsx;
export namespace JSX {
  export type Element = ReaktElement;
  export type ElementType = ((props: any) => ReaktNode) | symbol;
  export interface ElementChildrenAttribute { children: unknown }
  export interface IntrinsicAttributes { key?: string | number }
  export interface IntrinsicElements {}
}
