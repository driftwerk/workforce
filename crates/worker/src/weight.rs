//! What a scope weighs: which dev services it starts with, which it may start
//! on first use, and so what admitting it costs.
//!
//! The weight follows from what the scope was given. A scope with repos to
//! run is a coding session's and starts the services its goldens declare; one
//! with none, an agent turn's or a completion's, starts none and gets them
//! only if something in it reaches for them.

use workforce_agent::tools::exec_runner::RepoSpec;

use crate::config::WorkerConfig;

/// The value of `TOOLBOX_SERVICES` that starts nothing. toolbox-init fills an
/// empty value with its default set, so "none at seat" must be a name no
/// service has.
pub const NO_SERVICES: &str = "none";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Weight {
    /// No repos: the CLI, its login, its MCP bridge and its home.
    Light,
    /// Repos checked out, and the services they need.
    Full,
}

impl Weight {
    /// The weight of a scope holding `repos`.
    pub fn of(repos: &[RepoSpec]) -> Self {
        if repos.is_empty() {
            Self::Light
        } else {
            Self::Full
        }
    }

    /// The weight of a scope already prepared, from the repos it holds.
    pub fn of_held<K, V>(repos: &std::collections::BTreeMap<K, V>) -> Self {
        if repos.is_empty() {
            Self::Light
        } else {
            Self::Full
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Full => "full",
        }
    }
}

/// Which of the configured services a scope starts at seat and which it
/// leaves to start on first use, in toolbox-init's names and the configured
/// order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicePlan {
    pub eager: Vec<&'static str>,
    pub lazy: Vec<&'static str>,
    /// Whether the scope's own Docker engine waits for first use too: only a
    /// light scope's, since a full scope's setup may need it at seat.
    pub docker_lazy: bool,
}

impl ServicePlan {
    /// A light scope starts nothing at seat. A full one starts what the
    /// goldens of its repos declare, and everything when any of its repos
    /// has no golden or a golden that declares nothing, as every scope did
    /// before goldens could say.
    pub fn of(config: &WorkerConfig, repos: &[RepoSpec]) -> Self {
        let configured = config.service_names();
        let weight = Weight::of(repos);
        let wanted: Option<Vec<&str>> = match weight {
            _ if !config.lazy_services => None,
            Weight::Light => Some(Vec::new()),
            Weight::Full => repos
                .iter()
                .map(|repo| {
                    config
                        .golden_for_slug(&repo.slug)
                        .and_then(|golden| golden.services.as_ref())
                })
                .try_fold(Vec::new(), |mut wanted, declared| {
                    wanted.extend(declared?.iter().map(String::as_str));
                    Some(wanted)
                }),
        };
        let (eager, lazy) = match wanted {
            None => (configured, Vec::new()),
            Some(wanted) => configured
                .into_iter()
                .partition(|service| wanted.contains(service)),
        };
        Self {
            eager,
            lazy,
            docker_lazy: weight == Weight::Light && config.lazy_services,
        }
    }

    /// The toolbox's contract for this plan: what to start now, what to
    /// start on first use, and whether the scope's own Docker engine waits
    /// for first use too.
    pub fn env(&self) -> Vec<(String, String)> {
        let eager = match self.eager.is_empty() {
            true => NO_SERVICES.to_string(),
            false => self.eager.join(","),
        };
        let mut env = vec![("TOOLBOX_SERVICES".to_string(), eager)];
        if !self.lazy.is_empty() {
            env.push(("TOOLBOX_LAZY_SERVICES".to_string(), self.lazy.join(",")));
        }
        if self.docker_lazy {
            env.push(("TOOLBOX_DOCKER".to_string(), "lazy".to_string()));
        }
        env
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{tests::sample, GoldenConfig, Service};

    fn repo(alias: &str, slug: &str) -> RepoSpec {
        RepoSpec {
            alias: alias.into(),
            slug: slug.into(),
            default_branch: String::new(),
            setup: None,
            token: None,
        }
    }

    fn config() -> WorkerConfig {
        let mut config = sample();
        config.services = vec![Service::Redis, Service::Ministack];
        config.golden.push(GoldenConfig {
            services: Some(vec!["postgres".into(), "ministack".into()]),
            ..config.golden[0].clone()
        });
        config.golden[1].name = "kotlin".into();
        config.golden[1].slug = "acme/kotlin".into();
        config.golden.push(GoldenConfig {
            services: Some(Vec::new()),
            ..config.golden[0].clone()
        });
        config.golden[2].name = "web".into();
        config.golden[2].slug = "acme/web".into();
        config
    }

    fn plan(repos: &[RepoSpec]) -> (Vec<&'static str>, Vec<&'static str>) {
        let plan = ServicePlan::of(&config(), repos);
        (plan.eager, plan.lazy)
    }

    #[test]
    fn a_scope_without_repos_is_light_and_starts_nothing_at_seat() {
        assert_eq!(Weight::of(&[]), Weight::Light);
        assert_eq!(
            plan(&[]),
            (vec![], vec!["mysql", "postgres", "redis", "ministack"])
        );
        let env = ServicePlan::of(&config(), &[]).env();
        assert_eq!(
            env,
            [
                ("TOOLBOX_SERVICES".to_string(), "none".to_string()),
                (
                    "TOOLBOX_LAZY_SERVICES".to_string(),
                    "mysql,postgres,redis,ministack".to_string()
                ),
                ("TOOLBOX_DOCKER".to_string(), "lazy".to_string()),
            ],
            "an empty TOOLBOX_SERVICES would start toolbox-init's default set"
        );
    }

    #[test]
    fn a_full_scope_starts_what_its_goldens_declare_and_leaves_the_rest_lazy() {
        let kotlin = repo("kmono", "acme/kotlin");
        assert_eq!(Weight::of(std::slice::from_ref(&kotlin)), Weight::Full);
        assert_eq!(
            plan(std::slice::from_ref(&kotlin)),
            (vec!["postgres", "ministack"], vec!["mysql", "redis"])
        );
        let env = ServicePlan::of(&config(), &[kotlin]).env();
        assert!(!env.iter().any(|(key, _)| key == "TOOLBOX_DOCKER"));
        let env = ServicePlan::of(&config(), &[repo("web", "acme/web")]).env();
        assert!(
            !env.iter().any(|(key, _)| key == "TOOLBOX_DOCKER"),
            "a full scope's setup may need its engine at seat"
        );

        let web = repo("web", "ACME/Web");
        assert_eq!(
            plan(std::slice::from_ref(&web)),
            (vec![], vec!["mysql", "postgres", "redis", "ministack"]),
            "a golden declaring none starts none, matched by slug in any case"
        );
        assert_eq!(
            plan(&[web, repo("kmono", "acme/kotlin")]),
            (vec!["postgres", "ministack"], vec!["mysql", "redis"]),
            "several repos start the union, in the configured order"
        );
    }

    #[test]
    fn without_lazy_services_every_scope_starts_everything_as_before() {
        let config = WorkerConfig {
            lazy_services: false,
            ..config()
        };
        for repos in [vec![], vec![repo("web", "acme/web")]] {
            assert_eq!(
                ServicePlan::of(&config, &repos).env(),
                [(
                    "TOOLBOX_SERVICES".to_string(),
                    "mysql,postgres,redis,ministack".to_string()
                )]
            );
        }
    }

    #[test]
    fn a_repo_whose_golden_declares_nothing_or_has_none_starts_everything() {
        let everything = (vec!["mysql", "postgres", "redis", "ministack"], vec![]);
        assert_eq!(plan(&[repo("rails", "acme/rails")]), everything);
        assert_eq!(plan(&[repo("x", "acme/stranger")]), everything);
        assert_eq!(
            plan(&[repo("kmono", "acme/kotlin"), repo("x", "acme/stranger")]),
            everything
        );
        assert_eq!(
            ServicePlan::of(&config(), &[repo("rails", "acme/rails")]).env(),
            [(
                "TOOLBOX_SERVICES".to_string(),
                "mysql,postgres,redis,ministack".to_string()
            )]
        );
    }
}
