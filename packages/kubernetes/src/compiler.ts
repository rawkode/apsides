import type { DomainOptions } from "./model.ts";
import { renderTree } from "./runtime.ts";

/** This subpath is available only to the trusted compiler-generated wrapper. */
export const renderExport = (defaultExport: unknown, buildContext: { readonly provenance: DomainOptions["provenance"]; readonly sourceDigest?: string }): unknown => {
  const value = typeof defaultExport === "function" ? defaultExport(buildContext) : defaultExport;
  return renderTree(value, buildContext.provenance);
};
