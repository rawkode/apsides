declare module "node:fs" { export function readFileSync(path: URL, encoding: "utf8"): string; }
declare class URL { constructor(path: string, base?: string | URL); }
interface ImportMeta { readonly url: string; }
