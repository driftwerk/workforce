//! Native coding tools - read, edit, search, build, test, and commit code.
//!
//! This module provides 14 native coding tools that replace the external
//! `coding.delegate` tool (which spawned Claude Code CLI as a subprocess).
//!
//! ## Core Tools
//! - `code.read` - Read files with line numbers, ranges, glob patterns
//! - `code.edit` - Search-and-replace, multi-edit, create/delete files
//! - `code.search` - Full-text search via ripgrep
//! - `code.find` - Find files by name or glob pattern
//! - `code.find_replace` - Project-wide find and replace with preview/apply modes
//! - `code.refactor` - Structural edits: replace a definition by name, rename,
//!   diagnostics, references, definition
//! - `code.tree` - Project structure overview
//! - `code.symbols` - Language-aware symbol extraction
//! - `code.diff` - Git diff viewing
//!
//! ## Build/Test Tools
//! - `code.test` - Run tests with framework auto-detection
//! - `code.lint` - Run linters/formatters
//! - `code.build` - Build projects
//!
//! ## Git Tools
//! - `code.git` - Git operations (PR lifecycle handled by the github skill —
//!   `github.pr_create`, `github.pr_merge`, `github.list_pulls`)
//!
//! ## Ecosystem Tools
//! - `code.cargo` - Cargo ecosystem (audit, outdated, tree, geiger, deny)

pub mod build;
pub mod cargo;
pub mod compile_gate;
pub mod diff;
pub mod edit;
pub mod find;
pub mod find_replace;
pub mod git;
pub mod lint;
pub mod read;
pub mod refactor;
pub mod search;
pub mod symbols;
pub mod test;
pub mod tree;
pub mod utils;
pub mod worktree;

pub use build::CodeBuildTool;
pub use cargo::CodeCargoTool;
pub use diff::CodeDiffTool;
pub use edit::CodeEditTool;
pub use find::CodeFindTool;
pub use find_replace::CodeFindReplaceTool;
pub use git::CodeGitTool;
pub use lint::CodeLintTool;
pub use read::CodeReadTool;
pub use refactor::CodeRefactorTool;
pub use search::CodeSearchTool;
pub use symbols::CodeSymbolsTool;
pub use test::CodeTestTool;
pub use tree::CodeTreeTool;
