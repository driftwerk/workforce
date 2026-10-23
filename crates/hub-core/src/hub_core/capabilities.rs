//! Capabilities the gates check for: installing external skills, and who is an admin.

use crate::authz::Capability;
use serde::Deserialize;
use workforce_core::access::Visibility;
use workforce_skills::installer::{GithubRef, SkillOrigin};

#[derive(Deserialize)]
pub struct InstallExternalInput {
    #[serde(default)]
    pub github_repo: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub visibility: Option<String>,
}

impl InstallExternalInput {
    /// The value the caller passed and where it points, or what to pass instead.
    pub fn origin(&self) -> Result<(&str, SkillOrigin<'_>), String> {
        fn named(value: Option<&str>) -> Option<&str> {
            value.map(str::trim).filter(|v| !v.is_empty())
        }
        match (
            named(self.github_repo.as_deref()),
            named(self.source.as_deref()),
        ) {
            (Some(_), Some(_)) => {
                Err("Pass either `github_repo` or `source`, not both.".to_string())
            }
            (None, None) => Err(
                "Pass `github_repo` (`owner/repo[@ref][#subpath=dir]`) for a GitHub repository, \
                 or `source` for a skill directory in your workspace. With the skills_manage \
                 capability, `source` may also be any directory on the hub or a git URL on \
                 another host."
                    .to_string(),
            ),
            (Some(reference), None) => Some(GithubRef::parse(reference))
                .filter(GithubRef::is_well_formed)
                .map(|repo| (reference, SkillOrigin::Github(repo)))
                .ok_or_else(|| {
                    format!(
                        "`{reference}` is not a GitHub repository reference: `github_repo` takes \
                         `owner/repo`, optionally followed by `@<ref>` and `#subpath=<dir>`. Pass \
                         a git URL as `source` instead."
                    )
                }),
            (None, Some(source)) => Ok((source, SkillOrigin::from_source(source))),
        }
    }

    /// The visibility the caller asked for, if any.
    pub fn requested_visibility(&self) -> Result<Option<Visibility>, String> {
        match self.visibility.as_deref().map(str::trim) {
            None | Some("") => Ok(None),
            Some(raw) => Visibility::parse_word(raw).map(Some).ok_or_else(|| {
                format!("`{raw}` is not a visibility — use `private`, `team` or `everyone`.")
            }),
        }
    }
}

/// The capability an external install needs, classified with the tool's own input parser:
/// reading another git host is a skills manager's, anything else is authoring. Input the tool
/// rejects is authoring too, so the caller hears what to pass rather than a denial.
pub fn external_install_capability(input: &serde_json::Value) -> Capability {
    match serde_json::from_value::<InstallExternalInput>(input.clone())
        .as_ref()
        .map(InstallExternalInput::origin)
    {
        Ok(Ok((_, SkillOrigin::Git(_)))) => Capability::SkillsManage,
        _ => Capability::SkillsAuthor,
    }
}

/// Whether a `code.refactor` call needs a language server, which runs the
/// project's own build scripts and plugins: every action but
/// `replace_definition`, and any call whose action cannot be read.
pub fn runs_language_server(input: &serde_json::Value) -> bool {
    input.get("action").and_then(serde_json::Value::as_str) != Some("replace_definition")
}
