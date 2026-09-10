import { makeElement, type ApsidesElement, type ApsidesNode } from "./runtime.ts";
export { Fragment } from "./runtime.ts";
export const jsx = (type: unknown, props: unknown, _key?: unknown): ApsidesElement => { return makeElement(type, props); };
export const jsxs = jsx;
export namespace JSX {
  export type Element = ApsidesElement;
  export type ElementType = ((props: any) => ApsidesNode) | symbol;
  export interface ElementChildrenAttribute { children: unknown }
  export interface IntrinsicAttributes { key?: string | number }
  export interface IntrinsicElements {}
}
