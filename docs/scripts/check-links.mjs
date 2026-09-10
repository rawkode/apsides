import { access, readFile, readdir } from "node:fs/promises";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../dist/", import.meta.url));

const files = async (directory) => {
  const result = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) result.push(...await files(path));
    else result.push(path);
  }
  return result;
};

const allFiles = await files(root);
const htmlFiles = allFiles.filter((path) => path.endsWith(".html"));
const failures = [];

const outputPath = (pathname) => {
  const clean = decodeURIComponent(pathname).replace(/^\/+/, "");
  if (clean === "") return join(root, "index.html");
  if (clean.endsWith("/")) return join(root, clean, "index.html");
  if (/\.[a-z0-9]+$/i.test(clean)) return join(root, clean);
  return join(root, clean, "index.html");
};

for (const page of htmlFiles) {
  const html = await readFile(page, "utf8");
  const pagePath = `/${relative(root, page).replace(/\\/g, "/").replace(/index\.html$/, "")}`;
  const hrefs = [...html.matchAll(/\bhref=["']([^"']+)["']/g)].map((match) => match[1]);

  for (const href of hrefs) {
    if (/^(?:[a-z]+:|\/\/)/i.test(href) || href.startsWith("mailto:")) continue;
    const url = new URL(href, `https://docs.invalid${pagePath}`);
    const target = outputPath(url.pathname);
    try {
      await access(target);
    } catch {
      failures.push(`${pagePath} -> ${href} (missing target)`);
      continue;
    }

    if (url.hash && target.endsWith(".html")) {
      const targetHtml = await readFile(target, "utf8");
      const fragment = decodeURIComponent(url.hash.slice(1));
      const ids = new Set([...targetHtml.matchAll(/\bid=["']([^"']+)["']/g)].map((match) => match[1]));
      if (!ids.has(fragment)) failures.push(`${pagePath} -> ${href} (missing fragment)`);
    }
  }
}

if (failures.length > 0) {
  console.error(`Internal link check failed (${failures.length}):\n${failures.join("\n")}`);
  process.exitCode = 1;
} else {
  console.log(`Checked ${htmlFiles.length} HTML pages: all internal targets and fragments resolve.`);
}
