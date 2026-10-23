import { Link, useLocation } from "react-router";
import { DocsLayout } from "fumadocs-ui/layouts/docs";
import { DocsBody, DocsDescription, DocsPage, DocsTitle } from "fumadocs-ui/page";
import { RootProvider } from "fumadocs-ui/provider/react-router";
import type { TOCItemType } from "fumadocs-core/toc";
import { Home } from "lucide-react";
import { source } from "@/lib/docs-source";
import { useMDXComponents } from "@/components/docs/mdx-components";
import { useAuthStore } from "@/stores/auth-store";

const DOCS_PREFIX = "/docs";
const APP_HOME = "/";
const REPO_OWNER = "driftwerk";
const REPO_NAME = "workforce";

function pageSlugs(pathname: string): string[] {
  return pathname
    .slice(DOCS_PREFIX.length)
    .split("/")
    .filter((segment) => segment.length > 0);
}

function DocsShell({ children }: { children: React.ReactNode }) {
  return (
    <RootProvider search={{ enabled: false }} theme={{ enabled: false }}>
      <DocsLayout
        tree={source.pageTree}
        nav={{ title: "Workforce Docs", url: DOCS_PREFIX }}
        searchToggle={{ enabled: false }}
        themeSwitch={{ enabled: false }}
        links={[
          {
            text: "Back to app",
            url: APP_HOME,
            icon: <Home className="h-4 w-4" />,
            active: "none",
          },
        ]}
      >
        {children}
      </DocsLayout>
    </RootProvider>
  );
}

function DocsNotFound() {
  return (
    <DocsPage toc={[]}>
      <DocsTitle>Page not found</DocsTitle>
      <DocsDescription>
        We couldn't find a docs page at this path.
      </DocsDescription>
      <DocsBody>
        <p>
          Try the <Link to="/docs">docs home</Link> or pick a section from the
          sidebar.
        </p>
      </DocsBody>
    </DocsPage>
  );
}

export default function Docs() {
  const { pathname } = useLocation();
  const { identity } = useAuthStore();
  const page = source.getPage(pageSlugs(pathname));
  const mdxComponents = useMDXComponents();

  if (!page) {
    return (
      <DocsShell>
        <DocsNotFound />
      </DocsShell>
    );
  }

  const { title, description, toc, Body } = page.data;
  // workforce is a private repo; only roles with read access see the edit link.
  const canEdit = !identity || identity.role !== "member";
  const editOnGithub = canEdit
    ? { owner: REPO_OWNER, repo: REPO_NAME, sha: "main", path: `end-user-docs/${page.path}` }
    : undefined;

  return (
    <DocsShell>
      <DocsPage toc={toc as TOCItemType[]} editOnGithub={editOnGithub}>
        <DocsTitle>{title}</DocsTitle>
        {description ? <DocsDescription>{description}</DocsDescription> : null}
        <DocsBody>
          <Body components={mdxComponents} />
        </DocsBody>
      </DocsPage>
    </DocsShell>
  );
}
