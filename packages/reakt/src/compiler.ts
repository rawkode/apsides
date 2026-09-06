import type { DomainOptions } from "./model.ts";
import { renderTree } from "./runtime.ts";

/** This subpath is available only to the trusted compiler-generated wrapper. */
export function renderExport(defaultExport: unknown, buildContext: { readonly provenance: DomainOptions["provenance"]; readonly sourceDigest?: string }): { productionIR: unknown; fixtures: unknown } {
  const value = typeof defaultExport === "function" ? defaultExport(buildContext) : defaultExport;
  return { productionIR: renderTree(value, buildContext.provenance), fixtures: null };
}
