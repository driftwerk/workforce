//! Git branch + `gh pr create` orchestration.

use std::path::Path;

use eyre::{eyre, Context, Result};
use tokio::process::Command;

pub struct PrSpec {
    pub branch: String,
    pub title: String,
    pub body: String,
    pub draft: bool,
    pub paths: Vec<String>,
    pub commit_message: String,
}

/// Outcome of reconciling one page's curated content with its stable
/// curator branch / PR.
pub enum PrSync {
    /// No PR existed (or the last one merged); opened a fresh PR.
    Created(String),
    /// An open PR already existed; force-pushed the new content to it and
    /// brought its title, body and draft state up to this run.
    Updated(u64),
    /// A human closed the PR without merging — treated as "leave this
    /// page alone". Nothing pushed; requires human action to revive.
    SkippedClosed(u64),
    /// The curated content is identical to the current base; nothing to do.
    NoChange,
}

/// Where a page's stable curator branch stands on GitHub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrLifecycle {
    Open,
    Merged,
    ClosedUnmerged,
}

struct ExistingPr {
    number: u64,
    state: PrLifecycle,
    draft: bool,
}

/// Reconcile one page against its stable branch `docs/curator/{slug}`:
/// reuse the open PR (force-push) if one exists, skip if a human closed
/// it, otherwise open a fresh PR. The working tree must already hold the
/// curated edit for `spec.paths`, and HEAD must be on `main`.
pub async fn sync_pr(repo_root: &Path, spec: &PrSpec) -> Result<PrSync> {
    let existing = latest_pr_for_branch(repo_root, &spec.branch).await?;
    if let Some(pr) = &existing {
        if pr.state == PrLifecycle::ClosedUnmerged {
            return Ok(PrSync::SkippedClosed(pr.number));
        }
    }

    // Force-reset the branch to HEAD each run so the single PR stays
    // rebased on current main instead of drifting onto a stale base.
    reset_branch(repo_root, &spec.branch).await?;
    stage_paths(repo_root, &spec.paths).await?;
    if !has_staged_changes(repo_root).await? {
        return Ok(PrSync::NoChange);
    }
    commit(repo_root, &spec.commit_message).await?;
    force_push(repo_root, &spec.branch).await?;

    match existing {
        Some(pr) if pr.state == PrLifecycle::Open => {
            // The PR describes this run, not the first one: a stale body
            // misreports what changed and whether verification passed, and a
            // stale ready state lets a page that now breaks the build merge.
            edit_gh_pr(repo_root, pr.number, spec).await?;
            if let Some(args) = draft_change(pr.draft, spec.draft) {
                let number = pr.number.to_string();
                let mut ready = vec!["pr", "ready", number.as_str()];
                ready.extend_from_slice(args);
                run_gh(repo_root, &ready).await?;
            }
            Ok(PrSync::Updated(pr.number))
        }
        // None, or a previously-merged PR on this branch: open a new one.
        _ => Ok(PrSync::Created(create_gh_pr(repo_root, spec).await?)),
    }
}

async fn reset_branch(repo_root: &Path, name: &str) -> Result<()> {
    run_git(repo_root, &["checkout", "-B", name]).await
}

/// True when there are staged changes (i.e. the curated content differs
/// from the branch's base commit).
async fn has_staged_changes(repo_root: &Path) -> Result<bool> {
    let out = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(repo_root)
        .output()
        .await
        .wrap_err("git diff --cached --quiet")?;
    Ok(!out.status.success())
}

/// Most recent PR whose head is `branch`, across all states, or None.
async fn latest_pr_for_branch(repo_root: &Path, branch: &str) -> Result<Option<ExistingPr>> {
    let out = Command::new("gh")
        .args([
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "all",
            "--json",
            "number,state,isDraft",
            "--limit",
            "30",
            "--jq",
            // Highest number == most recent PR; emit nothing when there is none.
            "sort_by(.number) | last | select(. != null) | \"\\(.number) \\(.state) \\(.isDraft)\"",
        ])
        .current_dir(repo_root)
        .output()
        .await
        .wrap_err("gh pr list --head")?;
    if !out.status.success() {
        return Err(eyre!(
            "gh pr list failed (exit {}): {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr),
        ));
    }
    Ok(parse_pr_list_line(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse one `"<number> <STATE> <isDraft>"` line from the `gh pr list --jq`
/// output. Empty (no PR on the branch) yields None.
fn parse_pr_list_line(stdout: &str) -> Option<ExistingPr> {
    let mut fields = stdout.split_whitespace();
    let number: u64 = fields.next()?.parse().ok()?;
    let state = match fields.next()? {
        "OPEN" => PrLifecycle::Open,
        "MERGED" => PrLifecycle::Merged,
        "CLOSED" => PrLifecycle::ClosedUnmerged,
        _ => return None,
    };
    let draft = fields.next()? == "true";
    Some(ExistingPr {
        number,
        state,
        draft,
    })
}

/// The `gh pr ready` arguments that move a PR from draft state `current` to
/// `wanted`, or None when it is already there.
fn draft_change(current: bool, wanted: bool) -> Option<&'static [&'static str]> {
    match (current, wanted) {
        (true, false) => Some(&[]),
        (false, true) => Some(&["--undo"]),
        _ => None,
    }
}

async fn edit_gh_pr(repo_root: &Path, number: u64, spec: &PrSpec) -> Result<()> {
    let number = number.to_string();
    run_gh(
        repo_root,
        &[
            "pr",
            "edit",
            number.as_str(),
            "--title",
            &spec.title,
            "--body",
            &spec.body,
        ],
    )
    .await
}

async fn run_gh(repo_root: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new("gh")
        .args(args)
        .current_dir(repo_root)
        .output()
        .await
        .wrap_err_with(|| format!("spawn gh {}", args.join(" ")))?;
    if !out.status.success() {
        return Err(eyre!(
            "gh {} failed (exit {}): {}",
            args.iter().take(2).copied().collect::<Vec<_>>().join(" "),
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr),
        ));
    }
    Ok(())
}

async fn stage_paths(repo_root: &Path, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Err(eyre!("no paths to stage"));
    }
    let mut args: Vec<&str> = vec!["add", "--"];
    args.extend(paths.iter().map(String::as_str));
    run_git(repo_root, &args).await
}

async fn commit(repo_root: &Path, message: &str) -> Result<()> {
    run_git(repo_root, &["commit", "-m", message]).await
}

/// Publish the stable branch. If it already exists on the remote we
/// fetch it and force-push with a lease (rebasing the open PR onto the
/// new content without clobbering an unexpected update); if it doesn't,
/// a plain push creates it — `--force-with-lease` has no baseline to
/// check against for a fresh branch.
async fn force_push(repo_root: &Path, branch: &str) -> Result<()> {
    let on_remote = Command::new("git")
        .args(["fetch", "origin", branch])
        .current_dir(repo_root)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    let args: &[&str] = if on_remote {
        &["push", "--force-with-lease", "-u", "origin", branch]
    } else {
        &["push", "-u", "origin", branch]
    };
    run_git(repo_root, args).await
}

async fn create_gh_pr(repo_root: &Path, spec: &PrSpec) -> Result<String> {
    let mut args: Vec<String> = vec![
        "pr".into(),
        "create".into(),
        "--title".into(),
        spec.title.clone(),
        "--body".into(),
        spec.body.clone(),
        "--head".into(),
        spec.branch.clone(),
    ];
    if spec.draft {
        args.push("--draft".into());
    }
    let out = Command::new("gh")
        .args(&args)
        .current_dir(repo_root)
        .output()
        .await
        .wrap_err("spawn gh pr create")?;
    if !out.status.success() {
        return Err(eyre!(
            "gh pr create failed (exit {}): {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn run_git(repo_root: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
        .await
        .wrap_err_with(|| format!("spawn git {}", args.join(" ")))?;
    if !out.status.success() {
        return Err(eyre!(
            "git {} failed (exit {}): {}",
            args.join(" "),
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr),
        ));
    }
    Ok(())
}

/// Stable branch per page (or per bundle): one curator branch maps to
/// exactly one page, so re-curation force-updates the same PR instead of
/// opening a new one each run.
pub fn branch_name_for(pages: &[String]) -> String {
    if pages.len() == 1 {
        format!("docs/curator/{}", slugify(&pages[0]))
    } else {
        "docs/curator/bundle".to_string()
    }
}

fn slugify(page: &str) -> String {
    page.trim_end_matches(".mdx").replace('/', "-")
}

pub fn pr_title(pages: &[String]) -> String {
    match pages.len() {
        0 => "docs: curate (empty)".to_string(),
        1 => format!("docs: curate {}", pages[0]),
        n => format!("docs: curate {n} pages"),
    }
}

/// What the curator did to one page, for the pull request.
pub struct PageReport {
    pub page: String,
    pub sources: Vec<String>,
    /// The curator's own account of what changed and why.
    pub summary: String,
}

pub fn pr_body(
    pages: &[String],
    reports: &[PageReport],
    head_sha: &str,
    verification_note: Option<&str>,
) -> String {
    let mut body = String::with_capacity(1024);
    body.push_str(
        "Auto-generated by `workforce-docs-curator`. Force-updated on each run, \
         so inline comments may be marked outdated after a rebase.\n\n",
    );

    body.push_str("## Pages\n");
    for page in pages {
        body.push_str(&format!("- `end-user-docs/{page}`\n"));
    }
    body.push('\n');

    body.push_str("## What changed\n");
    for report in reports {
        body.push_str(&format!(
            "\n### `end-user-docs/{}`\n{}\n",
            report.page,
            report.summary.trim()
        ));
    }
    body.push('\n');

    body.push_str("## Sources referenced\n");
    for report in reports {
        body.push_str(&format!("\n### `end-user-docs/{}`\n", report.page));
        if report.sources.is_empty() {
            body.push_str("- (none)\n");
        } else {
            for src in &report.sources {
                body.push_str(&format!("- `{src}`\n"));
            }
        }
    }
    body.push('\n');

    body.push_str("## Verification\n");
    match verification_note {
        None => body.push_str("`npm run build` and `docs:verify-links` passed.\n"),
        Some(msg) => {
            body.push_str("Verification did not pass. Marked as Draft.\n\n```\n");
            body.push_str(msg);
            if !msg.ends_with('\n') {
                body.push('\n');
            }
            body.push_str("```\n");
        }
    }
    body.push('\n');

    body.push_str(&format!("Generated from `{head_sha}`.\n"));
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_single_page_is_stable_slug() {
        let b = branch_name_for(&["adapters/slack.mdx".into()]);
        assert_eq!(b, "docs/curator/adapters-slack");
    }

    #[test]
    fn branch_multi_page_is_bundle() {
        let b = branch_name_for(&["a.mdx".into(), "b.mdx".into()]);
        assert_eq!(b, "docs/curator/bundle");
    }

    #[test]
    fn title_singular_includes_path() {
        let t = pr_title(&["adapters/slack.mdx".into()]);
        assert_eq!(t, "docs: curate adapters/slack.mdx");
    }

    #[test]
    fn title_plural_uses_count() {
        let t = pr_title(&["a.mdx".into(), "b.mdx".into(), "c.mdx".into()]);
        assert_eq!(t, "docs: curate 3 pages");
    }

    #[test]
    fn body_lists_pages_and_sources_and_marks_verification() {
        let body = pr_body(
            &["adapters/slack.mdx".into()],
            &[PageReport {
                page: "adapters/slack.mdx".into(),
                sources: vec!["crates/skills/src/adapters/slack/lib.rs".into()],
                summary: "Documented the new `--thread` flag.".into(),
            }],
            "abc123",
            None,
        );
        assert!(body.contains("end-user-docs/adapters/slack.mdx"));
        assert!(body.contains("## What changed"));
        assert!(body.contains("Documented the new `--thread` flag."));
        assert!(body.contains("crates/skills/src/adapters/slack/lib.rs"));
        assert!(body.contains("`npm run build` and `docs:verify-links` passed"));
        assert!(body.contains("abc123"));
    }

    #[test]
    fn body_marks_draft_when_verification_failed() {
        let body = pr_body(
            &["x.mdx".into()],
            &[PageReport {
                page: "x.mdx".into(),
                sources: vec![],
                summary: "Fixed the title.".into(),
            }],
            "deadbee",
            Some("error: title missing"),
        );
        assert!(body.contains("Marked as Draft"));
        assert!(body.contains("error: title missing"));
    }

    #[test]
    fn slugify_converts_slashes_and_strips_mdx() {
        assert_eq!(slugify("adapters/slack.mdx"), "adapters-slack");
        assert_eq!(slugify("a/b/c.mdx"), "a-b-c");
        assert_eq!(slugify("index.mdx"), "index");
    }

    #[test]
    fn parse_pr_list_line_maps_each_state() {
        assert_eq!(
            parse_pr_list_line("160 OPEN false").unwrap().state,
            PrLifecycle::Open
        );
        assert_eq!(
            parse_pr_list_line("144 MERGED false").unwrap().state,
            PrLifecycle::Merged
        );
        assert_eq!(
            parse_pr_list_line("99 CLOSED true").unwrap().state,
            PrLifecycle::ClosedUnmerged
        );
        assert_eq!(parse_pr_list_line("160 OPEN false").unwrap().number, 160);
    }

    #[test]
    fn parse_pr_list_line_reads_the_draft_state() {
        assert!(parse_pr_list_line("21 OPEN true").unwrap().draft);
        assert!(!parse_pr_list_line("21 OPEN false").unwrap().draft);
        assert!(
            parse_pr_list_line("21 OPEN").is_none(),
            "a line without the draft state is not trusted"
        );
    }

    #[test]
    fn an_updated_pr_takes_the_draft_state_of_this_run() {
        // A page that now breaks the build turns a ready PR back into a draft.
        assert_eq!(draft_change(false, true), Some(&["--undo"][..]));
        // A page fixed since an earlier failing run becomes ready again.
        assert_eq!(draft_change(true, false), Some(&[][..]));
        assert_eq!(draft_change(true, true), None);
        assert_eq!(draft_change(false, false), None);
    }

    #[test]
    fn parse_pr_list_line_empty_is_none() {
        assert!(parse_pr_list_line("").is_none());
        assert!(parse_pr_list_line("  \n").is_none());
    }

    #[test]
    fn parse_pr_list_line_unknown_state_is_none() {
        assert!(parse_pr_list_line("12 LOCKED false").is_none());
    }

    #[test]
    fn parse_pr_list_line_non_numeric_is_none() {
        assert!(parse_pr_list_line("null null").is_none());
    }
}
