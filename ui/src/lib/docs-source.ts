import type { ComponentType } from "react";
import type { MDXComponents } from "mdx/types";
import { loader } from "fumadocs-core/source";
import { docs } from "collections";

export type MdxBody = ComponentType<{ components?: MDXComponents }>;

export interface DocPageData {
  title: string;
  description?: string;
  toc: unknown;
  Body: MdxBody;
}

export interface DocMetaData {
  title?: string;
  pages?: string[];
}

const loadedDocs = await Promise.all(
  Object.entries(docs.doc).map(async ([path, load]) => {
    const mod = await load();
    return {
      type: "page" as const,
      path,
      data: {
        title: mod.frontmatter.title,
        description: mod.frontmatter.description,
        toc: mod.toc,
        Body: mod.default,
      } satisfies DocPageData,
    };
  }),
);

const loadedMeta = await Promise.all(
  Object.entries(docs.meta).map(async ([path, load]) => ({
    type: "meta" as const,
    path,
    data: await load(),
  })),
);

export const source = loader<{ pageData: DocPageData; metaData: DocMetaData }>({
  baseUrl: "/docs",
  source: { files: [...loadedDocs, ...loadedMeta] },
});
