//! Two-phase cascade evaluator + write-time legal-targets validator.
//!
//! Hierarchy: System → Team → Agent → User.
//! Restrictiveness: `Allow < Ask < AlwaysAsk < Deny`.
//!
//! Admin-set rules at System / Team / Agent fold with
//! [`PermissionState::most_restrictive`] into a single `admin_state`.
//! The User-scope row then *overlays* `admin_state` with carve-out
//! semantics — the runtime "Always Allow" UX deliberately lets a user
//! ratchet *below* a parent `ask`, which is the whole reason `ask` and
//! `always_ask` are distinct states. See the plan doc for the full
//! match table.
//!
//! Write-time validation enforces the same legal-targets table the
//! runtime evaluator accepts, so the two stay in sync by construction.

use crate::types::{PermissionRule, PermissionScope, PermissionState};

/// Caller-supplied context for a single cascade evaluation. Everything
/// needed to look up the matching rule set across the hierarchy.
#[derive(Debug, Clone)]
pub struct CascadeRequest<'a> {
    /// User UUID stringified — matches `PermissionRule::scope_id` when
    /// `scope = User`.
    pub user_id: &'a str,
    /// Agent identifier (the existing `AgentId` newtype, stringified)
    /// — matches `scope = Agent`.
    pub agent_id: &'a str,
    /// Every team the user belongs to or that owns the agent. Order
    /// doesn't matter — the fold is associative.
    pub team_ids: &'a [String],
    /// Tool name to evaluate (e.g. `memory.recall`).
    pub tool_name: &'a str,
}

/// A System-level rule that was loaded from the YAML policy file
/// (`constitutional: true`). System rules never live in
/// `permission_rules`; the cascade receives them out-of-band so the
/// policy engine stays the single source of truth for that layer.
#[derive(Debug, Clone)]
pub struct SystemRule {
    pub tool_pattern: String,
    pub state: PermissionState,
}

/// Pure two-phase evaluator. Given the System rules (pre-loaded from
/// YAML) and the DB-backed rules for every Team / Agent / User scope
/// the caller assembled, return the effective state for the request.
pub fn evaluate(
    request: &CascadeRequest<'_>,
    system_rules: &[SystemRule],
    db_rules: &[PermissionRule],
) -> PermissionState {
    let admin_state = collect_admin_state(request, system_rules, db_rules);
    let user_state = collect_user_state(request, db_rules);
    overlay_user_choice(admin_state, user_state)
}

/// Fold System + Team + Agent rules into a single state using
/// `most_restrictive`. Starts from `Allow` so an empty rule set means
/// "no restriction declared anywhere."
fn collect_admin_state(
    request: &CascadeRequest<'_>,
    system_rules: &[SystemRule],
    db_rules: &[PermissionRule],
) -> PermissionState {
    let system_states = system_rules
        .iter()
        .filter(|r| tool_matches(&r.tool_pattern, request.tool_name))
        .map(|r| r.state);

    let admin_db_states = db_rules
        .iter()
        .filter(|r| !matches!(r.scope, PermissionScope::User))
        .filter(|r| scope_matches_request(r, request))
        .filter(|r| tool_matches(&r.tool_pattern, request.tool_name))
        .map(|r| r.state);

    system_states
        .chain(admin_db_states)
        .fold(PermissionState::Allow, PermissionState::most_restrictive)
}

/// Pick the user-scope row whose pattern matches. When more than one
/// row would match (e.g. exact pattern + wildcard), the most-specific
/// pattern wins; ties break to the most-restrictive state — so a
/// belt-and-braces "deny" is never relaxed by a co-existing "allow"
/// row.
fn collect_user_state(
    request: &CascadeRequest<'_>,
    db_rules: &[PermissionRule],
) -> Option<PermissionState> {
    db_rules
        .iter()
        .filter(|r| matches!(r.scope, PermissionScope::User))
        .filter(|r| r.scope_id == request.user_id)
        .filter(|r| tool_matches(&r.tool_pattern, request.tool_name))
        .max_by(|a, b| {
            pattern_specificity(&a.tool_pattern)
                .cmp(&pattern_specificity(&b.tool_pattern))
                .then_with(|| a.state.cmp(&b.state))
        })
        .map(|r| r.state)
}

/// The match table from the plan doc:
///
/// | admin_state | user_state    | effective |
/// |-------------|---------------|-----------|
/// | deny        | _             | deny      |
/// | always_ask  | Some(deny)    | deny      |
/// | always_ask  | _             | always_ask|
/// | ask         | Some(s)       | s         |
/// | ask         | None          | ask       |
/// | allow       | Some(s)       | s         |
/// | allow       | None          | allow     |
pub fn overlay_user_choice(
    admin_state: PermissionState,
    user_state: Option<PermissionState>,
) -> PermissionState {
    use PermissionState::*;
    match (admin_state, user_state) {
        (Deny, _) => Deny,
        (AlwaysAsk, Some(Deny)) => Deny,
        (AlwaysAsk, _) => AlwaysAsk,
        (Ask, Some(s)) | (Allow, Some(s)) => s,
        (Ask, None) => Ask,
        (Allow, None) => Allow,
    }
}

/// Set of states the caller is allowed to write at a given scope on
/// top of the supplied `admin_state`. Returned as a `Vec` rather than
/// a `HashSet` because the set is always small and the order matters
/// for UI rendering (least-restrictive first, so "Allow" is the
/// default-looking option).
pub fn legal_user_targets(admin_state: PermissionState) -> Vec<PermissionState> {
    use PermissionState::*;
    match admin_state {
        Deny => vec![],
        AlwaysAsk => vec![Deny],
        Ask => vec![Allow, Ask, AlwaysAsk, Deny],
        Allow => vec![Allow, Ask, AlwaysAsk, Deny],
    }
}

/// Write-time validator: is `target` a legal User-scope choice on top
/// of `admin_state`? Mirrors the runtime overlay match table so admins
/// can't smuggle in a row that the runtime would refuse to honour.
pub fn user_choice_is_legal(admin_state: PermissionState, target: PermissionState) -> bool {
    legal_user_targets(admin_state).contains(&target)
}

/// Tool-name pattern matcher — delegates to `workforce_policy::rules::tool_matches`
/// so the cascade, the policy engine, and approval routing share one matcher
/// (`*`, `prefix.*`, exact, and the reserved `@destructive` pattern).
pub fn tool_matches(pattern: &str, tool_name: &str) -> bool {
    workforce_policy::rules::tool_matches(pattern, tool_name)
}

/// Specificity score for tie-breaking when multiple patterns match
/// the same tool. Exact > prefix.* > * / @destructive. Higher is more specific.
pub fn pattern_specificity(pattern: &str) -> u32 {
    if pattern == "*" || pattern == workforce_policy::rules::DESTRUCTIVE_PATTERN {
        0
    } else if pattern.ends_with(".*") {
        1
    } else {
        2
    }
}

/// Does this DB rule's scope key match the request?
fn scope_matches_request(rule: &PermissionRule, request: &CascadeRequest<'_>) -> bool {
    match rule.scope {
        PermissionScope::Team => request.team_ids.iter().any(|t| t == &rule.scope_id),
        PermissionScope::Agent => rule.scope_id == request.agent_id,
        PermissionScope::User => rule.scope_id == request.user_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{PermissionOrigin, PermissionRule};

    fn admin_rule(
        scope: PermissionScope,
        scope_id: &str,
        tool: &str,
        state: PermissionState,
    ) -> PermissionRule {
        PermissionRule::new(scope, scope_id, tool, state, PermissionOrigin::Admin)
    }

    fn user_rule(uid: &str, tool: &str, state: PermissionState) -> PermissionRule {
        PermissionRule::new(
            PermissionScope::User,
            uid,
            tool,
            state,
            PermissionOrigin::UserRuntimeChoice,
        )
    }

    fn request<'a>(
        user: &'a str,
        agent: &'a str,
        teams: &'a [String],
        tool: &'a str,
    ) -> CascadeRequest<'a> {
        CascadeRequest {
            user_id: user,
            agent_id: agent,
            team_ids: teams,
            tool_name: tool,
        }
    }

    // ── overlay_user_choice: every (admin, user) combination ──

    #[test]
    fn overlay_admin_deny_dominates() {
        for user in [
            None,
            Some(PermissionState::Allow),
            Some(PermissionState::Ask),
            Some(PermissionState::AlwaysAsk),
            Some(PermissionState::Deny),
        ] {
            assert_eq!(
                overlay_user_choice(PermissionState::Deny, user),
                PermissionState::Deny
            );
        }
    }

    #[test]
    fn overlay_always_ask_accepts_only_deny_overlay() {
        assert_eq!(
            overlay_user_choice(PermissionState::AlwaysAsk, None),
            PermissionState::AlwaysAsk
        );
        assert_eq!(
            overlay_user_choice(PermissionState::AlwaysAsk, Some(PermissionState::Allow)),
            PermissionState::AlwaysAsk
        );
        assert_eq!(
            overlay_user_choice(PermissionState::AlwaysAsk, Some(PermissionState::Ask)),
            PermissionState::AlwaysAsk
        );
        assert_eq!(
            overlay_user_choice(PermissionState::AlwaysAsk, Some(PermissionState::AlwaysAsk)),
            PermissionState::AlwaysAsk
        );
        assert_eq!(
            overlay_user_choice(PermissionState::AlwaysAsk, Some(PermissionState::Deny)),
            PermissionState::Deny
        );
    }

    #[test]
    fn overlay_ask_passes_through_user_choice() {
        for s in [
            PermissionState::Allow,
            PermissionState::Ask,
            PermissionState::AlwaysAsk,
            PermissionState::Deny,
        ] {
            assert_eq!(overlay_user_choice(PermissionState::Ask, Some(s)), s);
        }
        assert_eq!(
            overlay_user_choice(PermissionState::Ask, None),
            PermissionState::Ask
        );
    }

    #[test]
    fn overlay_allow_passes_through_user_choice() {
        for s in [
            PermissionState::Allow,
            PermissionState::Ask,
            PermissionState::AlwaysAsk,
            PermissionState::Deny,
        ] {
            assert_eq!(overlay_user_choice(PermissionState::Allow, Some(s)), s);
        }
        assert_eq!(
            overlay_user_choice(PermissionState::Allow, None),
            PermissionState::Allow
        );
    }

    // ── legal_user_targets and write-time validator ──

    #[test]
    fn legal_targets_match_match_table() {
        assert!(legal_user_targets(PermissionState::Deny).is_empty());
        assert_eq!(
            legal_user_targets(PermissionState::AlwaysAsk),
            vec![PermissionState::Deny]
        );
        let any_state = vec![
            PermissionState::Allow,
            PermissionState::Ask,
            PermissionState::AlwaysAsk,
            PermissionState::Deny,
        ];
        assert_eq!(legal_user_targets(PermissionState::Ask), any_state.clone());
        assert_eq!(legal_user_targets(PermissionState::Allow), any_state);
    }

    #[test]
    fn legal_user_targets_and_runtime_agree() {
        // The "same answer either way" invariant from the plan:
        //   * legal target  → runtime honours the user's choice exactly
        //   * illegal target → user row is indistinguishable from no row
        //     (the runtime falls back to the admin state)
        let states = [
            PermissionState::Allow,
            PermissionState::Ask,
            PermissionState::AlwaysAsk,
            PermissionState::Deny,
        ];
        for admin in states {
            let fallback = overlay_user_choice(admin, None);
            for target in states {
                let effective = overlay_user_choice(admin, Some(target));
                if user_choice_is_legal(admin, target) {
                    assert_eq!(
                        effective, target,
                        "legal target {target:?} under admin {admin:?} must be honoured",
                    );
                } else {
                    assert_eq!(
                        effective, fallback,
                        "illegal target {target:?} under admin {admin:?} must collapse to no-row fallback {fallback:?}",
                    );
                }
            }
        }
    }

    // ── tool_matches ──

    #[test]
    fn tool_matches_exact_prefix_and_wildcard() {
        assert!(tool_matches("*", "anything"));
        assert!(tool_matches("memory.*", "memory.recall"));
        assert!(!tool_matches("memory.*", "memory")); // must be > prefix
        assert!(!tool_matches("memory.*", "memoryxrecall")); // not a dot boundary
        assert!(tool_matches("vault.delete", "vault.delete"));
        assert!(!tool_matches("vault.delete", "vault.list"));
    }

    #[test]
    fn pattern_specificity_orders_exact_over_prefix_over_wildcard() {
        assert!(pattern_specificity("vault.delete") > pattern_specificity("vault.*"));
        assert!(pattern_specificity("vault.*") > pattern_specificity("*"));
    }

    // ── full evaluate path ──

    #[test]
    fn evaluate_returns_allow_when_no_rules_match() {
        let req = request("u", "a", &[], "memory.recall");
        assert_eq!(evaluate(&req, &[], &[]), PermissionState::Allow);
    }

    #[test]
    fn system_destructive_baseline_upgrades_only_destructive_tools() {
        let system = vec![SystemRule {
            tool_pattern: workforce_policy::rules::DESTRUCTIVE_PATTERN.to_string(),
            state: PermissionState::Ask,
        }];
        assert_eq!(
            evaluate(&request("u", "a", &[], "jira_delete_issue"), &system, &[]),
            PermissionState::Ask
        );
        assert_eq!(
            evaluate(&request("u", "a", &[], "jira_get_issue"), &system, &[]),
            PermissionState::Allow
        );
    }

    #[test]
    fn evaluate_system_rule_caps_team_choice() {
        // System=Ask is a ceiling. Even if a team mistakenly writes
        // Allow into the DB, the cascade still says Ask.
        let team = "team-a".to_string();
        let req = request("u", "a", std::slice::from_ref(&team), "memory.recall");
        let system = vec![SystemRule {
            tool_pattern: "memory.*".to_string(),
            state: PermissionState::Ask,
        }];
        let db = vec![admin_rule(
            PermissionScope::Team,
            &team,
            "memory.recall",
            PermissionState::Allow,
        )];
        assert_eq!(evaluate(&req, &system, &db), PermissionState::Ask);
    }

    #[test]
    fn evaluate_user_allow_overrides_team_ask() {
        // The case the advisor flagged: Team=Ask, User clicked
        // Always-Allow → User=Allow → cascade must return Allow so the
        // user isn't re-prompted.
        let team = "team-a".to_string();
        let req = request("u", "a", std::slice::from_ref(&team), "memory.recall");
        let db = vec![
            admin_rule(
                PermissionScope::Team,
                &team,
                "memory.*",
                PermissionState::Ask,
            ),
            user_rule("u", "memory.recall", PermissionState::Allow),
        ];
        assert_eq!(evaluate(&req, &[], &db), PermissionState::Allow);
    }

    #[test]
    fn evaluate_user_allow_cannot_relax_always_ask() {
        let team = "team-a".to_string();
        let req = request("u", "a", std::slice::from_ref(&team), "memory.recall");
        let db = vec![
            admin_rule(
                PermissionScope::Team,
                &team,
                "memory.*",
                PermissionState::AlwaysAsk,
            ),
            user_rule("u", "memory.recall", PermissionState::Allow),
        ];
        // User's allow is ignored — always_ask is the floor for permissive choices.
        assert_eq!(evaluate(&req, &[], &db), PermissionState::AlwaysAsk);
    }

    #[test]
    fn evaluate_user_deny_overrides_always_ask() {
        let team = "team-a".to_string();
        let req = request("u", "a", std::slice::from_ref(&team), "memory.recall");
        let db = vec![
            admin_rule(
                PermissionScope::Team,
                &team,
                "memory.*",
                PermissionState::AlwaysAsk,
            ),
            user_rule("u", "memory.recall", PermissionState::Deny),
        ];
        assert_eq!(evaluate(&req, &[], &db), PermissionState::Deny);
    }

    #[test]
    fn evaluate_admin_chain_takes_most_restrictive_across_levels() {
        // System=Allow, Team=Ask, Agent=AlwaysAsk → admin_state=AlwaysAsk.
        let team = "team-a".to_string();
        let req = request("u", "agent-1", std::slice::from_ref(&team), "memory.recall");
        let db = vec![
            admin_rule(
                PermissionScope::Team,
                &team,
                "memory.*",
                PermissionState::Ask,
            ),
            admin_rule(
                PermissionScope::Agent,
                "agent-1",
                "memory.*",
                PermissionState::AlwaysAsk,
            ),
        ];
        assert_eq!(evaluate(&req, &[], &db), PermissionState::AlwaysAsk);
    }

    #[test]
    fn evaluate_ignores_rules_for_other_scopes() {
        let team_a = "team-a".to_string();
        let req = request(
            "u",
            "agent-1",
            std::slice::from_ref(&team_a),
            "memory.recall",
        );
        // Rules for a team the user isn't in, an agent they're not
        // using, and a different user must not contribute.
        let db = vec![
            admin_rule(
                PermissionScope::Team,
                "team-b",
                "memory.*",
                PermissionState::Deny,
            ),
            admin_rule(
                PermissionScope::Agent,
                "agent-2",
                "memory.*",
                PermissionState::Deny,
            ),
            user_rule("other-user", "memory.recall", PermissionState::Deny),
        ];
        assert_eq!(evaluate(&req, &[], &db), PermissionState::Allow);
    }

    #[test]
    fn evaluate_user_most_specific_pattern_wins_with_restrictive_tiebreak() {
        // Two User-scope rules match the same tool: exact wins over
        // wildcard. The state on the more-specific rule is honoured.
        let req = request("u", "a", &[], "memory.recall");
        let db = vec![
            user_rule("u", "memory.*", PermissionState::Allow),
            user_rule("u", "memory.recall", PermissionState::Deny),
        ];
        assert_eq!(evaluate(&req, &[], &db), PermissionState::Deny);

        // Equal specificity ⇒ most-restrictive wins.
        let db = vec![
            user_rule("u", "memory.recall", PermissionState::Ask),
            user_rule("u", "memory.recall", PermissionState::AlwaysAsk),
        ];
        assert_eq!(evaluate(&req, &[], &db), PermissionState::AlwaysAsk);
    }

    #[test]
    fn evaluate_tool_pattern_must_match_to_contribute() {
        let team = "team-a".to_string();
        let req = request("u", "a", std::slice::from_ref(&team), "memory.recall");
        let db = vec![admin_rule(
            PermissionScope::Team,
            &team,
            "vault.*", // wrong namespace
            PermissionState::Deny,
        )];
        assert_eq!(evaluate(&req, &[], &db), PermissionState::Allow);
    }
}
