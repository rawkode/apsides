import { makeElement, type ReaktElement } from "./runtime.ts";
export { Fragment } from "./runtime.ts";
export type { JSX } from "./jsx-runtime.ts";
export function jsxDEV(type: unknown, props: unknown, _key?: unknown, _staticChildren?: boolean, source?: { fileName: string; lineNumber: number; columnNumber: number }, _self?: unknown): ReaktElement {
  return makeElement(type, props, source);
}
