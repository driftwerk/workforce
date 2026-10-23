# Developer docs

Engineer reference material that predates the end-user documentation
system. The files in this directory are **possibly stale** and should not be
treated as canonical truth for either users or the doc-curator agent.

Canonical end-user documentation lives in `end-user-docs/` and is rendered
in-app at `/docs`. The curator drafts those pages from current source code
and configuration schemas — not from this directory.

If you reach for something in here:

- Cross-check the claim against the code before relying on it.
- If the page is still useful and end-user-facing, port the relevant
  content into the corresponding `end-user-docs/` page via a PR.
- If it's purely for contributors (architecture deep dives, development
  workflow), leaving it here is fine.

This directory will shrink over time as content is migrated or retired.
