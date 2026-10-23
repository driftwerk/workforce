-- Recent workflow instances, newest first: the pipeline runs view filters by
-- workflow, status and a since time, then orders by update and takes a page.
-- A bare workflow id also matches member copies (`{owner}/{id}`), so the
-- workflow column cannot lead; the update time serves every such read.
CREATE INDEX IF NOT EXISTS idx_workflow_instances_updated ON workflow_instances(updated_at);
