export type Child = Element | readonly Child[] | null | undefined | boolean;
export type Component<P = any> = (props: P) => Child;

export interface Element {
  readonly type: Component | typeof Fragment;
  readonly props: Record<string, any>;
  readonly key?: string | number;
}

export const Fragment = Symbol("Fragment");

/** JSX only records descriptions. Components execute later, inside compile(). */
export function jsx(type: Element["type"], props: Record<string, any> | null, key?: string | number): Element {
  return { type, props: props ?? {}, key };
}

export const jsxs = jsx;
export const jsxDEV = jsx;

export namespace JSX {
  export type Element = import("./jsx-runtime").Element;
  export type ElementType = Component;
  export interface ElementChildrenAttribute { children: {} }
  export interface IntrinsicAttributes { key?: string | number }
  export interface IntrinsicElements {}
}
