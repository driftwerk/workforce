/// <reference types="vite/client" />

declare module "collections" {
  import type { ComponentType } from "react";
  import type { MDXComponents } from "mdx/types";

  type CompiledMDXFile = {
    frontmatter: { title: string; description?: string };
    toc: unknown;
    structuredData: unknown;
    default: ComponentType<{ components?: MDXComponents }>;
  };

  type DocsMetaFile = { title?: string; pages?: string[] };

  export const docs: {
    doc: Record<string, (() => Promise<CompiledMDXFile>) & { base: string }>;
    meta: Record<string, (() => Promise<DocsMetaFile>) & { base: string }>;
  };
}
