export const Fragment = Symbol.for("oxideloop.fragment");
export const ExplicitEmpty = Symbol.for("oxideloop.explicit-empty");
export interface Element { readonly type: unknown; readonly props: Record<string, unknown>; readonly key?: string; }
export function jsx(type: unknown, props: Record<string, unknown>, key?: string): Element { return { type, props, ...(key === undefined ? {} : { key }) }; }
export const jsxs = jsx;
export function explicitEmpty(): Element { return { type: ExplicitEmpty, props: {} }; }
