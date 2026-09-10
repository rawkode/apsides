import { makeElement, type ApsidesElement } from "./runtime.ts";
export { Fragment } from "./runtime.ts";
export type { JSX } from "./jsx-runtime.ts";
export const jsxDEV = (type: unknown, props: unknown, _key?: unknown, _staticChildren?: boolean, source?: { fileName: string; lineNumber: number; columnNumber: number }, _self?: unknown): ApsidesElement => {
  return makeElement(type, props, source);
};
