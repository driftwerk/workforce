#!/usr/bin/env node
import { readdir, readFile } from "node:fs/promises";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const docsRoot = resolve(here, "..", "..", "end-user-docs");
const linkPattern = /\((\/docs(?:\/[^)#?\s]+)?)(?:#[^)]*)?\)/g;

async function* walk(dir) {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) yield* walk(full);
    else if (entry.isFile() && /\.(mdx?|md)$/.test(entry.name)) yield full;
  }
}

async function collectSlugs() {
  const slugs = new Set(["/docs"]);
  for await (const file of walk(docsRoot)) {
    const rel = relative(docsRoot, file).replace(/\.mdx?$/, "");
    const segments = rel.split("/").filter((s) => s !== "index");
    slugs.add(["/docs", ...segments].join("/").replace(/\/$/, "") || "/docs");
  }
  return slugs;
}

async function main() {
  const valid = await collectSlugs();
  const broken = [];
  for await (const file of walk(docsRoot)) {
    const text = await readFile(file, "utf8");
    for (const [, href] of text.matchAll(linkPattern)) {
      const normalized = href.replace(/\/$/, "") || "/docs";
      if (!valid.has(normalized)) {
        broken.push({ file: relative(docsRoot, file), href });
      }
    }
  }
  if (broken.length > 0) {
    console.error(`Found ${broken.length} broken /docs link(s):`);
    for (const b of broken) console.error(`  ${b.file}: ${b.href}`);
    process.exit(1);
  }
  console.log(`verify-docs-links: ${valid.size} pages, no broken /docs/ links.`);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
