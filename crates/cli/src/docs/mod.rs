//! `workforce docs curate` — doc-curator entrypoint.
//!
//! Per page, the prompt embeds the source files (or notes read from them)
//! and the current page, and the model changes the page through edit tools
//! (see `llm`). The dispatcher is workforce's existing LLM provider
//! abstraction (`workforce_agent::llm`); pick the provider via the
//! `[curator]` section of `workforce.toml` or the per-invocation `--tier`
//! flag.
//!
//! `gh` and `git` are invoked via subprocess.

mod check;
mod cli_help;
mod edit;
mod llm;
mod pr;
mod prompt;
mod verify;
mod watch_config;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Args, Subcommand};
use eyre::{eyre, Context, Result};
use tokio::process::Command;
use tracing::{info, warn};

use workforce_common::settings::WorkforceConfig;
use workforce_hub::vault::Vault;

use self::llm::{curate_page, Curation};
use self::pr::{branch_name_for, pr_body, pr_title, sync_pr, PageReport, PrSpec, PrSync};
use self::prompt::{collect_valid_doc_paths, read_page, read_source_files, PromptInput};
use self::verify::{verify, VerificationOutcome};
use self::watch_config::{all_pages, load_watch_config, pages_for_changed_files, sources_for_page};

const DEFAULT_MAX_PAGES: usize = 5;
const SYSTEM_PROMPT: &str = include_str!("../../curator/system-prompt.md");

#[derive(Subcommand, Debug)]
pub enum DocsCommand {
    /// Curate one or more pages with the doc-curator agent.
    Curate(CurateArgs),
    /// Check pages' examples: every `workforce` command must parse with the
    /// real command line, and every `workforce.toml` block must load into
    /// the real config. Exits non-zero when any example fails.
    Check(CheckArgs),
}

#[derive(Args, Debug)]
pub struct CheckArgs {
    /// Page paths relative to `end-user-docs/`; every page when omitted.
    pub pages: Vec<String>,
}

#[derive(Args, Debug)]
pub struct CurateArgs {
    /// Page paths relative to `end-user-docs/` (e.g. `adapters/slack.mdx`).
    pub pages: Vec<String>,

    /// Curate every page declared in `docs-watch.toml`.
    #[arg(long)]
    pub all: bool,

    /// Curate pages whose source globs match files changed in `<ref>..HEAD`.
    #[arg(long, value_name = "REF")]
    pub changed_since: Option<String>,

    /// Print the plan, write nothing, no PR.
    #[arg(long)]
    pub dry_run: bool,

    /// Write changes to a local branch but don't push or open a PR.
    #[arg(long)]
    pub no_pr: bool,

    /// One bundled PR for all curated pages (default: one PR per page).
    #[arg(long)]
    pub bundle: bool,

    /// Cap the number of pages curated in this run.
    #[arg(long, default_value_t = DEFAULT_MAX_PAGES)]
    pub max_pages: usize,

    /// Override the tier spec from `[curator]` for this invocation.
    /// Example: `openrouter:deepseek:powerful`, `anthropic:powerful`.
    #[arg(long)]
    pub tier: Option<String>,

    /// Cap on each edit reply (overrides `[curator] max_tokens`).
    #[arg(long)]
    pub max_tokens: Option<u32>,

    /// Allow the working tree to be dirty when invoked.
    #[arg(long)]
    pub allow_dirty: bool,

    /// Workforce config path (used for `[curator]` defaults + vault).
    #[arg(long, default_value = "workforce.toml")]
    pub config: PathBuf,
}

pub async fn run(command: DocsCommand) -> Result<()> {
    match command {
        DocsCommand::Curate(args) => curate(args).await,
        DocsCommand::Check(args) => check_pages(args),
    }
}

async fn curate(args: CurateArgs) -> Result<()> {
    let repo_root = std::env::current_dir().wrap_err("cwd")?;
    let watch_path = repo_root.join("end-user-docs").join("docs-watch.toml");
    let cfg = load_watch_config(&watch_path)?;

    let mut targets = resolve_targets(&args, &cfg, &repo_root).await?;
    if targets.is_empty() {
        info!("no pages to curate; exiting");
        return Ok(());
    }
    if targets.len() > args.max_pages {
        warn!(
            target_count = targets.len(),
            max = args.max_pages,
            "capping run to --max-pages",
        );
        targets.truncate(args.max_pages);
    }

    if args.dry_run {
        println!("Would curate {} page(s):", targets.len());
        for page in &targets {
            println!("  - {page}");
        }
        return Ok(());
    }

    if !args.allow_dirty {
        ensure_clean(&repo_root).await?;
    }

    let (mut curator_config, vault) = load_curator_config(&args.config).await?;
    let tier_spec = args
        .tier
        .clone()
        .or(curator_config.tier.clone())
        .ok_or_else(|| {
            eyre!(
                "no curator tier configured: set `[curator] tier = \"<provider:tier>\"` \
                 in workforce.toml (e.g. `openrouter:deepseek:powerful`) or pass --tier",
            )
        })?;
    if let Some(max_tokens) = args.max_tokens {
        curator_config.max_tokens = max_tokens;
    }
    let valid_doc_paths = collect_valid_doc_paths(&repo_root.join("end-user-docs"))?;

    let mut pages_touched: Vec<PageOutcome> = Vec::with_capacity(targets.len());
    for page in &targets {
        let outcome = match curate_single_page(
            page,
            &repo_root,
            &cfg,
            &tier_spec,
            &curator_config,
            &valid_doc_paths,
            vault.as_ref(),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(err) => {
                warn!(page, error = %format_args!("{err:#}"), "curate failed; skipping this page, continuing batch");
                PageOutcome {
                    page: page.clone(),
                    sources: Vec::new(),
                    summary: String::new(),
                    still_failing: Vec::new(),
                    status: PageStatus::Skipped,
                }
            }
        };
        pages_touched.push(outcome);
    }

    let verification = verify(&repo_root.join("ui")).await?;
    surface_verification(&verification);

    if args.no_pr {
        info!("--no-pr: leaving changes uncommitted in working tree");
        return Ok(());
    }

    let head_sha = git_head_sha(&repo_root).await?;
    open_pull_requests(
        &pages_touched,
        &repo_root,
        &head_sha,
        args.bundle,
        &verification,
    )
    .await?;

    Ok(())
}

fn check_pages(args: CheckArgs) -> Result<()> {
    let repo_root = std::env::current_dir().wrap_err("cwd")?;
    let docs_root = repo_root.join("end-user-docs");
    let cfg = load_watch_config(&docs_root.join("docs-watch.toml"))?;
    let pages = if args.pages.is_empty() {
        all_mdx_pages(&docs_root)?
    } else {
        args.pages
    };
    let mut failed = 0;
    let (mut commands, mut configs, mut unchecked) = (0, 0, 0);
    for page in &pages {
        let text = std::fs::read_to_string(docs_root.join(page))
            .wrap_err_with(|| format!("read end-user-docs/{page}"))?;
        let report = check::check_page(&text, cfg.toml_schema(page));
        commands += report.commands_checked;
        configs += report.config_blocks_checked;
        unchecked += report.config_blocks_unchecked;
        for finding in &report.findings {
            println!("end-user-docs/{page}: {finding}");
        }
        failed += report.findings.len();
    }
    println!(
        "{} pages: {commands} commands and {configs} config blocks checked, \
         {unchecked} config blocks for files without a checker, {failed} failing",
        pages.len()
    );
    if failed > 0 {
        return Err(eyre!("{failed} examples fail their check"));
    }
    Ok(())
}

fn all_mdx_pages(docs_root: &Path) -> Result<Vec<String>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
        for entry in std::fs::read_dir(dir).wrap_err_with(|| format!("read {}", dir.display()))? {
            let path = entry?.path();
            if path.is_dir() {
                walk(root, &path, out)?;
            } else if path.extension().is_some_and(|e| e == "mdx") {
                let rel = path.strip_prefix(root).unwrap_or(&path);
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(docs_root, docs_root, &mut out)?;
    out.sort();
    Ok(out)
}

/// What happened to a page during a run, used to decide whether it gets a PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageStatus {
    /// The curator finished an edit and the page was written.
    Edited,
    /// The curator judged the page already accurate — no PR.
    Unchanged,
    /// No mapped sources, or the curation failed and nothing was written —
    /// no PR.
    Skipped,
}

struct PageOutcome {
    page: String,
    sources: Vec<String>,
    /// The curator's account of what changed, for the PR.
    summary: String,
    /// Examples that failed their check before the edit and still do.
    still_failing: Vec<String>,
    status: PageStatus,
}

async fn resolve_targets(
    args: &CurateArgs,
    cfg: &watch_config::WatchConfig,
    repo_root: &Path,
) -> Result<Vec<String>> {
    let pages: BTreeSet<String> = if !args.pages.is_empty() {
        if args.all || args.changed_since.is_some() {
            return Err(eyre!(
                "explicit page arguments cannot be combined with --all or --changed-since",
            ));
        }
        for page in &args.pages {
            if page.contains("..") || Path::new(page).is_absolute() {
                return Err(eyre!(
                    "page paths must be relative to end-user-docs/: {page}",
                ));
            }
        }
        args.pages.iter().cloned().collect()
    } else if args.all {
        if args.changed_since.is_some() {
            return Err(eyre!("--all and --changed-since are mutually exclusive"));
        }
        all_pages(cfg)
    } else if let Some(reference) = &args.changed_since {
        let changed = changed_files(repo_root, reference).await?;
        pages_for_changed_files(cfg, &changed)?
    } else {
        return Err(eyre!(
            "specify one or more page arguments, --all, or --changed-since=<ref>",
        ));
    };
    Ok(pages.into_iter().collect())
}

#[allow(clippy::too_many_arguments)]
async fn curate_single_page(
    page: &str,
    repo_root: &Path,
    cfg: &watch_config::WatchConfig,
    tier_spec: &str,
    limits: &workforce_common::settings::CuratorConfig,
    valid_doc_paths: &[String],
    vault: Option<&Arc<Vault>>,
) -> Result<PageOutcome> {
    let page_full_path = repo_root.join("end-user-docs").join(page);
    let source_paths = sources_for_page(cfg, page, repo_root)?;
    if source_paths.is_empty() {
        warn!(page, "no source files mapped in docs-watch.toml; skipping");
        return Ok(PageOutcome {
            page: page.to_string(),
            sources: Vec::new(),
            summary: String::new(),
            still_failing: Vec::new(),
            status: PageStatus::Skipped,
        });
    }

    let sources = match cfg.page_source(page) {
        Some(watch_config::PageSource::CliHelp) => vec![prompt::SourceFile {
            repo_relative: PathBuf::from("workforce --help, every subcommand"),
            content: cli_help::cli_help_reference(),
        }],
        None => read_source_files(&source_paths, repo_root)?,
    };
    let current_content = read_page(&page_full_path)?;
    let source_summary: Vec<String> = sources
        .iter()
        .map(|s| s.repo_relative.display().to_string())
        .collect();

    let prompt_input = PromptInput {
        page: page.to_string(),
        current_content,
        sources,
        valid_doc_paths: valid_doc_paths.to_vec(),
    };

    info!(page, tier = tier_spec, "curating page");
    let curation = curate_page(
        page,
        tier_spec,
        limits,
        SYSTEM_PROMPT,
        &prompt_input,
        cfg.toml_schema(page),
        vault,
    )
    .await?;
    let Curation::Edited {
        text,
        summary,
        edits,
        model,
        input_tokens,
        output_tokens,
        still_failing,
    } = curation
    else {
        if let Curation::Unchanged { reason } = curation {
            info!(page, %reason, "curator judged page already accurate; skipping");
        }
        return Ok(unchanged(page, source_summary));
    };

    if let Some(parent) = page_full_path.parent() {
        std::fs::create_dir_all(parent).wrap_err_with(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(&page_full_path, &text)
        .wrap_err_with(|| format!("write {}", page_full_path.display()))?;

    info!(
        page,
        %model,
        edits,
        input_tokens,
        output_tokens,
        still_failing = still_failing.len(),
        "page edited"
    );

    Ok(PageOutcome {
        page: page.to_string(),
        sources: source_summary,
        summary,
        still_failing,
        status: PageStatus::Edited,
    })
}

fn unchanged(page: &str, sources: Vec<String>) -> PageOutcome {
    PageOutcome {
        page: page.to_string(),
        sources,
        summary: String::new(),
        still_failing: Vec::new(),
        status: PageStatus::Unchanged,
    }
}

async fn open_pull_requests(
    outcomes: &[PageOutcome],
    repo_root: &Path,
    head_sha: &str,
    bundle: bool,
    verification: &VerificationOutcome,
) -> Result<()> {
    let edited: Vec<&PageOutcome> = outcomes
        .iter()
        .filter(|o| o.status == PageStatus::Edited)
        .collect();
    if edited.is_empty() {
        info!("no pages changed; not opening a PR");
        return Ok(());
    }

    // sync_pr force-creates the branch from HEAD, so each call must start on main.
    if bundle {
        let pages: Vec<String> = edited.iter().map(|o| o.page.clone()).collect();
        let page_refs: Vec<&str> = pages.iter().map(String::as_str).collect();
        let note = verification.failure_summary(&page_refs);
        let spec = build_pr_spec(
            &pages,
            &page_reports(&edited),
            head_sha,
            note.as_deref(),
            verification.build_failed,
        );
        checkout_main(repo_root).await?;
        report_sync(&pages.join(", "), sync_pr(repo_root, &spec).await?);
    } else {
        for &outcome in &edited {
            let pages = [outcome.page.clone()];
            let note = verification.page_failure(&outcome.page);
            let spec = build_pr_spec(
                &pages,
                &page_reports(&[outcome]),
                head_sha,
                note.as_deref(),
                verification.build_failed,
            );
            checkout_main(repo_root).await?;
            report_sync(&outcome.page, sync_pr(repo_root, &spec).await?);
        }
    }
    checkout_main(repo_root).await
}

fn page_reports(outcomes: &[&PageOutcome]) -> Vec<PageReport> {
    outcomes
        .iter()
        .map(|o| PageReport {
            page: o.page.clone(),
            sources: o.sources.clone(),
            summary: o.summary.clone(),
            still_failing: o.still_failing.clone(),
        })
        .collect()
}

fn report_sync(label: &str, sync: PrSync) {
    match sync {
        PrSync::Created(url) => println!("opened PR for {label}: {url}"),
        PrSync::Updated(number) => println!("updated PR #{number} for {label}"),
        PrSync::SkippedClosed(number) => warn!(
            pr = number,
            page = label,
            "curator PR was closed without merging; leaving it alone. \
             Delete its `docs/curator/*` branch to let the curator reopen it",
        ),
        PrSync::NoChange => info!(page = label, "content matches base; nothing pushed"),
    }
}

fn build_pr_spec(
    pages: &[String],
    reports: &[PageReport],
    head_sha: &str,
    verification_note: Option<&str>,
    build_failed: bool,
) -> PrSpec {
    let title = pr_title(pages);
    // Draft whenever this page broke verification OR the docs build failed at
    // all — even unattributed. `vite` stops at the first bad MDX, so later
    // broken pages (and good pages in a batch with a pre-existing broken page)
    // carry no per-page marker; without this they'd open as ready-to-merge PRs
    // that look green but break `main`.
    let fallback_note = "The docs build (`npm run build`) did not pass for this batch. The error \
        may be on another page (vite stops at the first bad MDX) or on a page already \
        broken on `main`. Opened as a draft — fix the build before merging.";
    let effective_note = match (verification_note, build_failed) {
        (Some(n), _) => Some(n.to_string()),
        (None, true) => Some(fallback_note.to_string()),
        (None, false) => None,
    };
    PrSpec {
        branch: branch_name_for(pages),
        body: pr_body(pages, reports, head_sha, effective_note.as_deref()),
        draft: effective_note.is_some(),
        paths: pages.iter().map(|p| format!("end-user-docs/{p}")).collect(),
        commit_message: title.clone(),
        title,
    }
}

/// Load the `[curator]` section and (if configured) open the vault.
/// Both are optional: when `workforce.toml` is absent, we return
/// defaults and rely on the CLI `--tier` override + the relevant
/// provider env var (e.g. `OPENROUTER_API_KEY`) being set.
async fn load_curator_config(
    config_path: &Path,
) -> Result<(
    workforce_common::settings::CuratorConfig,
    Option<Arc<Vault>>,
)> {
    if !config_path.exists() {
        return Ok((workforce_common::settings::CuratorConfig::default(), None));
    }
    let wf_config = WorkforceConfig::load(config_path)
        .wrap_err_with(|| format!("load {}", config_path.display()))?;
    let curator = wf_config.curator.clone();
    let vault = workforce_hub::vault::open_global(&wf_config).await?;
    Ok((curator, vault))
}

async fn ensure_clean(repo_root: &Path) -> Result<()> {
    let out = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo_root)
        .output()
        .await
        .wrap_err("git status")?;
    if !out.status.success() {
        return Err(eyre!(
            "git status failed: {}",
            String::from_utf8_lossy(&out.stderr),
        ));
    }
    if !out.stdout.is_empty() {
        return Err(eyre!(
            "working tree is dirty; commit/stash or pass --allow-dirty",
        ));
    }
    Ok(())
}

async fn changed_files(repo_root: &Path, reference: &str) -> Result<Vec<String>> {
    let range = format!("{reference}..HEAD");
    let out = Command::new("git")
        .args(["diff", "--name-only", &range])
        .current_dir(repo_root)
        .output()
        .await
        .wrap_err("git diff --name-only")?;
    if !out.status.success() {
        return Err(eyre!(
            "git diff failed: {}",
            String::from_utf8_lossy(&out.stderr),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect())
}

async fn git_head_sha(repo_root: &Path) -> Result<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_root)
        .output()
        .await
        .wrap_err("git rev-parse HEAD")?;
    if !out.status.success() {
        return Err(eyre!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&out.stderr),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn checkout_main(repo_root: &Path) -> Result<()> {
    let out = Command::new("git")
        .args(["checkout", "main"])
        .current_dir(repo_root)
        .output()
        .await
        .wrap_err("git checkout main")?;
    if !out.status.success() {
        return Err(eyre!(
            "git checkout main failed: {}",
            String::from_utf8_lossy(&out.stderr),
        ));
    }
    Ok(())
}

fn surface_verification(outcome: &VerificationOutcome) {
    if !outcome.any_failure() {
        info!("verification passed");
        return;
    }
    let pages: BTreeSet<&str> = outcome
        .broken_links_by_page
        .keys()
        .chain(outcome.mdx_errors_by_page.keys())
        .map(String::as_str)
        .collect();
    warn!(
        failed_pages = ?pages,
        "verification failed; affected PRs will be marked Draft with their per-page errors",
    );
    eprintln!("--- verification output ---\n{}", outcome.raw_output);
}
