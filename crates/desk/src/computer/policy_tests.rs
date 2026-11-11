//! The verdict, rule by rule. Each row also meets a later rule's conditions,
//! so that a rule checked out of order fails here.

use std::path::{Path, PathBuf};

use uuid::Uuid;
use workforce_core::computer::{
    Ability, Access, Always, Asking, By, Choice, ComputerAccess, Effect, Refused, Verdict, Why,
};

use super::policy::*;

const OWN: &str = "/Users/sarah/Desk/hub/agents/allan";
const ACME: &str = "/Users/sarah/dev/acme-web";
const DATA: &str = "/Users/sarah/Library/Application Support/Desk";

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

fn policy(access: Access) -> AgentPolicy {
    AgentPolicy {
        hub: "local".into(),
        agent: "allan".into(),
        name: "Allan".into(),
        access: ComputerAccess {
            access,
            autonomous: false,
            ask_in_own_folder: false,
            unattended: Default::default(),
            asks: Default::default(),
        },
        own: vec![p(OWN)],
        works_in: vec![],
        withheld: vec![p(DATA), p("/Users/sarah/Library/Keychains")],
        config: vec![
            p(&format!("{OWN}/config.toml")),
            p(&format!("{OWN}/SOUL.md")),
        ],
        scratch: vec![p("/private/tmp")],
        home: p("/Users/sarah"),
        contained: false,
        connectors: None,
        browser: Browser::default(),
    }
}

fn full() -> AgentPolicy {
    policy(Access::Full)
}

fn autonomous() -> AgentPolicy {
    let mut p = full();
    p.access.autonomous = true;
    p
}

fn call(effect: Effect) -> CallFacts {
    CallFacts {
        tool: "filesystem.write".into(),
        effect,
        person: Some("sarah".into()),
        session_id: Some("s1".into()),
        run: None,
        folder: None,
    }
}

fn in_folder(effect: Effect, folder: &str) -> CallFacts {
    CallFacts {
        folder: Some(p(folder)),
        ..call(effect)
    }
}

fn read(path: &str) -> Effect {
    Effect::Read {
        paths: vec![p(path)],
    }
}

fn write(path: &str) -> Effect {
    Effect::Write {
        paths: vec![p(path)],
    }
}

fn delete(path: &str) -> Effect {
    Effect::Delete {
        paths: vec![p(path)],
    }
}

fn exec(command: &str, cwd: &str) -> Effect {
    Effect::Exec {
        command: command.into(),
        cwd: Some(p(cwd)),
    }
}

fn connector(tool: &str, id: &str, write: bool) -> CallFacts {
    CallFacts {
        tool: tool.into(),
        ..call(Effect::Connector {
            connector: id.into(),
            write,
        })
    }
}

fn grant(ability: Ability, folder: &str) -> Grant {
    Grant {
        id: Uuid::from_u128(1),
        hub: "local".into(),
        agent: "allan".into(),
        person: "sarah".into(),
        ability,
        folder: Some(p(folder)),
        connector: None,
        session_id: None,
        run: None,
    }
}

fn session_grant(ability: Ability, folder: &str, session: &str) -> Grant {
    Grant {
        id: Uuid::from_u128(2),
        session_id: Some(session.into()),
        ..grant(ability, folder)
    }
}

fn connector_grant(id: &str) -> Grant {
    Grant {
        id: Uuid::from_u128(3),
        ability: Ability::ConnectorWrite,
        folder: None,
        connector: Some(id.into()),
        ..grant(Ability::ConnectorWrite, "/")
    }
}

fn refused(v: &Verdict) -> Option<(Refused, &str)> {
    match v {
        Verdict::Refuse { refusal } => Some((refusal.rule, refusal.sentence.as_str())),
        _ => None,
    }
}

fn run() -> Verdict {
    Verdict::Run { by: By::Rule }
}

fn once(why: Why) -> Verdict {
    Verdict::Ask {
        choices: vec![Choice::AllowOnce, Choice::Deny],
        why,
    }
}

fn asks(always: Option<Always>, why: Why) -> Verdict {
    let mut choices = vec![Choice::AllowOnce, Choice::AllowSession];
    choices.extend(always.map(Choice::Always));
    choices.push(Choice::Deny);
    Verdict::Ask { choices, why }
}

#[test]
fn a_call_with_no_effect_here_runs_even_when_stopped() {
    let v = verdict(&full(), true, &[], &call(Effect::None));
    assert_eq!(v, run());
}

#[test]
fn rule_1_the_kill_switch_refuses_before_everything_else() {
    // Withheld, outside the level, granted: still "stopped".
    let mut low = policy(Access::OwnFolder);
    low.access.ask_in_own_folder = true;
    for (policy, effect) in [
        (full(), read(&format!("{DATA}/hub.db"))),
        (low.clone(), write(ACME)),
        (full(), read(&format!("{OWN}/notes.md"))),
        (full(), exec("npm test", ACME)),
    ] {
        let v = verdict(
            &policy,
            true,
            &[grant(Ability::Write, ACME), grant(Ability::Run, ACME)],
            &call(effect),
        );
        assert_eq!(
            refused(&v),
            Some((
                Refused::Stopped,
                "Access to this computer is stopped, so nothing was read, changed or run. Allow it again in Settings → This computer."
            ))
        );
    }
}

#[test]
fn the_kill_switch_does_not_stop_connectors() {
    let v = verdict(
        &full(),
        true,
        &[],
        &connector("linear.issue_get", "linear", false),
    );
    assert_eq!(v, run());
}

#[test]
fn rule_2_the_withheld_set_is_refused_at_every_level_whatever_is_granted() {
    let config = format!("{OWN}/config.toml");
    for policy in [full(), autonomous(), policy(Access::ReadAnywhere)] {
        for effect in [
            read(&format!("{DATA}/vault.db")),
            write(&config),
            delete("/Users/sarah/Library/Keychains/login.keychain-db"),
            exec("ls", DATA),
            Effect::Move {
                from: vec![p(&format!("{OWN}/a"))],
                to: p(&config),
            },
        ] {
            let v = verdict(
                &policy,
                false,
                &[
                    grant(Ability::Write, OWN),
                    grant(Ability::Run, "/Users/sarah/Library"),
                ],
                &call(effect.clone()),
            );
            assert_eq!(
                refused(&v).map(|r| r.0),
                Some(Refused::Withheld),
                "{effect:?}"
            );
        }
    }
    let v = verdict(&full(), false, &[], &call(write(&config)));
    assert_eq!(
        refused(&v).unwrap().1,
        "Allan can't use ~/Desk/hub/agents/allan/config.toml: Desk keeps it from every agent."
    );
}

/// The own folder may lie inside Desk's data: it stays the agent's, while
/// the rest (other agents, the vault) is withheld. Its config files are
/// read as before, never changed.
#[test]
fn the_own_folder_is_carved_out_of_the_withheld_set_and_its_config_only_read() {
    let data = "/Users/sarah/Library/Application Support/Desk";
    let own = format!("{data}/hub/data/agents/allan");
    let mut inside = full();
    inside.own = vec![p(&own)];
    inside.config = vec![p(&format!("{own}/config.toml"))];
    assert_eq!(
        verdict(
            &inside,
            false,
            &[],
            &call(write(&format!("{own}/notes.md")))
        ),
        run()
    );
    assert_eq!(
        verdict(
            &inside,
            false,
            &[],
            &call(read(&format!("{own}/config.toml")))
        ),
        run()
    );
    for effect in [
        write(&format!("{own}/config.toml")),
        read(&format!("{data}/hub/data/agents/ben/notes.md")),
        read(&format!("{data}/hub/data/vault/global.db")),
        exec("ls", &format!("{data}/hub")),
    ] {
        let v = verdict(&inside, false, &[], &call(effect.clone()));
        assert_eq!(
            refused(&v).map(|r| r.0),
            Some(Refused::Withheld),
            "{effect:?}"
        );
    }
}

#[test]
fn rule_3_outside_the_level_refuses_and_no_grant_widens_a_level() {
    let granted = [
        grant(Ability::Write, ACME),
        grant(Ability::Run, ACME),
        grant(Ability::Delete, ACME),
    ];
    let own = policy(Access::OwnFolder);
    let anywhere = policy(Access::ReadAnywhere);
    for (policy, effect, sentence) in [
        (
            &own,
            read(&format!("{ACME}/README.md")),
            "Allan can only read its own folder at Own folder. To let it read ~/dev/acme-web/README.md, change its access in Settings → This computer.",
        ),
        (
            &own,
            write(&format!("{ACME}/a.rs")),
            "Allan can only change its own folder at Own folder. To let it change ~/dev/acme-web/a.rs, change its access in Settings → This computer.",
        ),
        (
            &anywhere,
            delete(&format!("{ACME}/a.rs")),
            "Allan can only change its own folder at Read anywhere. To let it change ~/dev/acme-web/a.rs, change its access in Settings → This computer.",
        ),
        (
            &anywhere,
            Effect::Move {
                from: vec![p(&format!("{OWN}/a"))],
                to: p(&format!("{ACME}/a")),
            },
            "Allan can only change its own folder at Read anywhere. To let it change ~/dev/acme-web/a, change its access in Settings → This computer.",
        ),
        (
            &own,
            exec("npm test", OWN),
            "Allan can't run commands at Own folder on this computer. Choose Full access in Settings → This computer to let it.",
        ),
        (
            &anywhere,
            Effect::Session { cwd: Some(p(OWN)) },
            "Allan can't start sessions at Read anywhere on this computer. Choose Full access in Settings → This computer to let it.",
        ),
        (
            &anywhere,
            Effect::Unknown,
            "Allan can only change its own folder at Read anywhere, and this tool can't say what it changes. To let it, change its access in Settings → This computer.",
        ),
    ] {
        let v = verdict(policy, false, &granted, &in_folder(effect.clone(), ACME));
        assert_eq!(refused(&v), Some((Refused::OutsideLevel, sentence)), "{effect:?}");
    }
}

#[test]
fn rule_3_a_contained_command_must_start_in_the_own_folder_at_own_folder() {
    let mut own = policy(Access::OwnFolder);
    own.contained = true;
    let v = verdict(
        &own,
        false,
        &[grant(Ability::Run, ACME)],
        &call(exec("npm test", ACME)),
    );
    assert_eq!(
        refused(&v),
        Some((
            Refused::OutsideLevel,
            "Allan can only run commands in its own folder at Own folder. To let it go further, change its access in Settings → This computer."
        ))
    );
    assert_eq!(
        verdict(&own, false, &[], &call(exec("npm test", OWN))),
        run()
    );
    let mut anywhere = policy(Access::ReadAnywhere);
    anywhere.contained = true;
    assert_eq!(
        verdict(&anywhere, false, &[], &call(exec("npm test", ACME))),
        run()
    );
    // Sessions aren't contained: they start in the own folder at a lower level.
    let v = verdict(
        &anywhere,
        false,
        &[],
        &call(Effect::Session { cwd: Some(p(ACME)) }),
    );
    assert_eq!(refused(&v).map(|r| r.0), Some(Refused::OutsideLevel));
}

#[test]
fn rule_3_reads_reach_anywhere_above_own_folder() {
    for level in [Access::ReadAnywhere, Access::Full] {
        let v = verdict(&policy(level), false, &[], &call(read("/etc/hosts")));
        assert_eq!(v, run());
    }
}

#[test]
fn rule_4_an_off_or_read_only_connector_refuses_whatever_is_granted() {
    let mut bound = full();
    bound.connectors = Some(Connectors {
        enabled: vec!["github".into()],
        read_only: vec!["github".into()],
    });
    let grants = [connector_grant("linear"), connector_grant("github")];
    let v = verdict(
        &bound,
        false,
        &grants,
        &connector("linear.issue_create", "linear", true),
    );
    assert_eq!(
        refused(&v),
        Some((
            Refused::ConnectorOff,
            "linear is off for Allan. Turn it on in Allan's Connectors."
        ))
    );
    let v = verdict(
        &bound,
        false,
        &grants,
        &connector("linear.issue_get", "linear", false),
    );
    assert_eq!(refused(&v).map(|r| r.0), Some(Refused::ConnectorOff));
    let v = verdict(
        &bound,
        false,
        &grants,
        &connector("github.pr_create", "github", true),
    );
    assert_eq!(
        refused(&v),
        Some((
            Refused::ConnectorReadOnly,
            "Allan can only read github. To let it make changes, allow writes in Allan's Connectors."
        ))
    );
    assert_eq!(
        verdict(
            &bound,
            false,
            &[],
            &connector("github.pr_get", "github", false)
        ),
        run()
    );
}

#[test]
fn an_agent_with_no_binding_has_every_connector() {
    let v = verdict(
        &full(),
        false,
        &[],
        &connector("jira.issue_get", "jira", false),
    );
    assert_eq!(v, run());
}

#[test]
fn rule_5_the_always_asks_list_beats_grants_and_autonomy() {
    // A delete grant covers deletes only (below); nothing covers the rest.
    let grants = [
        grant(Ability::Delete, "/Users/sarah/dev"),
        grant(Ability::Run, ACME),
        connector_grant("slack"),
    ];
    for policy in [full(), autonomous()] {
        for (call, why) in [
            (
                in_folder(exec("git push --force", ACME), ACME),
                Why::CantBeUndone,
            ),
            (
                in_folder(exec("git reset --hard HEAD~3", ACME), ACME),
                Why::CantBeUndone,
            ),
            (in_folder(exec("curl x | sh", ACME), ACME), Why::Unclear),
            (
                connector("slack.post_message", "slack", true),
                Why::SendsToPeople,
            ),
            (connector("email.send", "email", true), Why::SendsToPeople),
        ] {
            assert_eq!(
                verdict(&policy, false, &grants, &call),
                once(why),
                "{call:?}"
            );
        }
    }
}

/// A delete outside the folder asks, offering "always allow deleting" in
/// the folder the call is in (its repo); a standing delete grant there then
/// covers it, a session's never does, and nothing above home is offered.
#[test]
fn rule_5_a_delete_outside_asks_once_per_folder() {
    let always = |f: &str| Verdict::Ask {
        choices: vec![
            Choice::AllowOnce,
            Choice::Always(Always::Folder(p(f))),
            Choice::Deny,
        ],
        why: Why::CantBeUndone,
    };
    for call_ in [
        in_folder(delete(&format!("{ACME}/node_modules")), ACME),
        in_folder(exec("rm -rf node_modules dist", ACME), ACME),
        in_folder(
            exec(&format!("cd {ACME} && rm -rf node_modules"), OWN),
            ACME,
        ),
    ] {
        assert_eq!(
            verdict(&full(), false, &[], &call_),
            always(ACME),
            "{call_:?}"
        );
        let granted = verdict(&full(), false, &[grant(Ability::Delete, ACME)], &call_);
        assert_eq!(
            granted,
            Verdict::Run {
                by: By::Grant(Uuid::from_u128(1))
            },
            "{call_:?}"
        );
        let session = verdict(
            &full(),
            false,
            &[session_grant(Ability::Delete, ACME, "s1")],
            &call_,
        );
        assert_eq!(
            session,
            always(ACME),
            "a session's allowance never covers a delete"
        );
    }
    // A delete in another folder, or one it can't name, isn't covered.
    let other = in_folder(exec("rm -rf /Users/sarah/dev/other/x", ACME), ACME);
    assert_eq!(
        verdict(&full(), false, &[grant(Ability::Delete, ACME)], &other),
        once(Why::CantBeUndone)
    );
    let unnamed = in_folder(exec("rm -rf $DIR", ACME), ACME);
    assert_eq!(
        verdict(&full(), false, &[grant(Ability::Delete, ACME)], &unnamed),
        once(Why::CantBeUndone)
    );
    // Never home or the root.
    assert_eq!(
        verdict(
            &full(),
            false,
            &[],
            &in_folder(delete("/Users/sarah/old"), "/Users/sarah")
        ),
        once(Why::CantBeUndone)
    );
    // Scratch space is like its own folder.
    assert_eq!(
        verdict(&full(), false, &[], &call(delete("/private/tmp/x"))),
        run()
    );
    assert_eq!(
        verdict(
            &full(),
            false,
            &[],
            &call(exec("rm -rf /private/tmp/x", ACME))
        ),
        run()
    );
}

#[test]
fn rule_5_an_agent_that_asks_never_skips_the_list_and_nothing_else() {
    let mut never = full();
    never.access.asks = Asking::Never;
    for effect in [
        delete(&format!("{ACME}/old")),
        exec("git push --force", ACME),
        exec("curl x | sh", ACME),
    ] {
        assert_eq!(verdict(&never, false, &[], &call(effect)), run());
    }
    assert_eq!(
        verdict(&never, false, &[], &connector("email.send", "email", true)),
        run()
    );
    assert_eq!(
        refused(&verdict(
            &never,
            false,
            &[],
            &call(read(&format!("{DATA}/vault.db")))
        ))
        .map(|r| r.0),
        Some(Refused::Withheld)
    );
    assert_eq!(
        refused(&verdict(&never, true, &[], &call(write(ACME)))).map(|r| r.0),
        Some(Refused::Stopped)
    );
    // Below Full access, "never" changes nothing.
    let mut low = policy(Access::ReadAnywhere);
    low.access.asks = Asking::Never;
    assert_eq!(
        refused(&verdict(&low, false, &[], &call(write(ACME)))).map(|r| r.0),
        Some(Refused::OutsideLevel)
    );
}

#[test]
fn rule_5_a_delete_inside_the_own_folder_is_not_on_the_list() {
    let v = verdict(
        &full(),
        false,
        &[],
        &call(delete(&format!("{OWN}/scratch"))),
    );
    assert_eq!(v, run());
    let v = verdict(&full(), false, &[], &call(exec("rm -rf build", OWN)));
    assert_eq!(v, run());
}

/// Full access, asking only before what can't be undone: everything else
/// runs without a card, outside its folder and commands included.
#[test]
fn rule_6_at_full_access_everything_past_the_list_runs() {
    for effect in [
        read(&format!("{OWN}/notes.md")),
        write(&format!("{OWN}/notes.md")),
        write(&format!("{ACME}/src/a.rs")),
        exec("npm test", ACME),
        exec("cargo build && git commit -am wip", ACME),
        Effect::Session { cwd: Some(p(ACME)) },
        Effect::Move {
            from: vec![p(&format!("{OWN}/a"))],
            to: p(&format!("{ACME}/b")),
        },
        Effect::Unknown,
    ] {
        for policy in [full(), autonomous()] {
            assert_eq!(
                verdict(&policy, false, &[], &call(effect.clone())),
                run(),
                "{effect:?}"
            );
        }
    }
    assert_eq!(
        verdict(
            &full(),
            false,
            &[],
            &connector("linear.issue_create", "linear", true)
        ),
        run()
    );
    let mut asking = full();
    asking.access.ask_in_own_folder = true;
    assert_eq!(
        verdict(&asking, false, &[], &call(write(&format!("{OWN}/a")))),
        run()
    );
}

#[test]
fn rule_6_reads_and_own_folder_changes_run_at_a_lower_level() {
    for effect in [
        read(&format!("{OWN}/notes.md")),
        write(&format!("{OWN}/notes.md")),
        Effect::Move {
            from: vec![p(&format!("{OWN}/a"))],
            to: p(&format!("{OWN}/b")),
        },
    ] {
        assert_eq!(
            verdict(&policy(Access::OwnFolder), false, &[], &call(effect)),
            run()
        );
    }
}

/// Below Full access, with its own folder (here its work trees too) set to
/// ask: where grants and asking are reached now.
fn asking() -> AgentPolicy {
    let mut p_ = policy(Access::ReadAnywhere);
    p_.own = vec![p(OWN), p("/Users/sarah/dev"), p("/Volumes/Backup")];
    p_.access.ask_in_own_folder = true;
    p_.contained = true;
    p_
}

#[test]
fn rule_6_ask_in_own_folder_sends_own_folder_changes_on_to_grants_and_asking() {
    let effect = write(&format!("{OWN}/notes.md"));
    assert_eq!(
        verdict(&asking(), false, &[], &in_folder(effect.clone(), OWN)),
        asks(Some(Always::Folder(p(OWN))), Why::InOwnFolder)
    );
    assert_eq!(
        verdict(
            &asking(),
            false,
            &[grant(Ability::Write, OWN)],
            &call(effect)
        ),
        Verdict::Run {
            by: By::Grant(Uuid::from_u128(1))
        }
    );
    assert_eq!(
        verdict(&asking(), false, &[], &call(read(&format!("{OWN}/a")))),
        run()
    );
}

#[test]
fn rule_7_a_grant_covers_its_ability_folder_and_scope_only() {
    let target = write(&format!("{ACME}/src/a.rs"));
    let ran = |by| Verdict::Run { by };
    assert_eq!(
        verdict(
            &asking(),
            false,
            &[grant(Ability::Write, ACME)],
            &call(target.clone())
        ),
        ran(By::Grant(Uuid::from_u128(1)))
    );
    assert_eq!(
        verdict(
            &asking(),
            false,
            &[session_grant(Ability::Write, ACME, "s1")],
            &call(target.clone())
        ),
        ran(By::Session(Uuid::from_u128(2)))
    );
    let not_covering = [
        grant(Ability::Run, ACME),
        grant(Ability::Write, "/Users/sarah/dev/acme"),
        grant(Ability::Write, &format!("{ACME}/docs")),
        session_grant(Ability::Write, ACME, "s2"),
        Grant {
            person: "tom".into(),
            ..grant(Ability::Write, ACME)
        },
        Grant {
            agent: "ben".into(),
            ..grant(Ability::Write, ACME)
        },
        Grant {
            hub: "acme".into(),
            ..grant(Ability::Write, ACME)
        },
        Grant {
            run: Some(RunOf {
                kind: "job".into(),
                id: "j1".into(),
            }),
            ..grant(Ability::Write, ACME)
        },
    ];
    for g in not_covering {
        let v = verdict(
            &asking(),
            false,
            std::slice::from_ref(&g),
            &call(target.clone()),
        );
        assert!(matches!(v, Verdict::Ask { .. }), "{g:?} → {v:?}");
    }
    // A job's grant covers that job's run.
    let job = RunOf {
        kind: "job".into(),
        id: "j1".into(),
    };
    let jobs = Grant {
        run: Some(job.clone()),
        ..grant(Ability::Write, ACME)
    };
    let in_job = CallFacts {
        run: Some(job),
        session_id: None,
        ..call(target.clone())
    };
    assert_eq!(
        verdict(&asking(), false, &[jobs], &in_job),
        ran(By::Grant(Uuid::from_u128(1)))
    );
    // A move needs both its delete and its write covered.
    let moving = Effect::Move {
        from: vec![p(&format!("{ACME}/a"))],
        to: p(&format!("{ACME}/b")),
    };
    assert!(matches!(
        verdict(
            &asking(),
            false,
            &[grant(Ability::Write, ACME)],
            &call(moving.clone())
        ),
        Verdict::Ask { .. }
    ));
    assert_eq!(
        verdict(
            &asking(),
            false,
            &[grant(Ability::Write, ACME), grant(Ability::Delete, ACME)],
            &call(moving)
        ),
        ran(By::Grant(Uuid::from_u128(1)))
    );
    // A connector write grant (a connector write asks below Full access).
    assert_eq!(
        verdict(
            &asking(),
            false,
            &[connector_grant("linear")],
            &connector("linear.issue_create", "linear", true)
        ),
        ran(By::Grant(Uuid::from_u128(3)))
    );
}

#[test]
fn rule_8_is_not_reached_while_full_access_runs_past_the_list() {
    assert_eq!(
        verdict(
            &autonomous(),
            false,
            &[],
            &in_folder(write(&format!("{ACME}/a")), ACME)
        ),
        run()
    );
    // Autonomy written below full (admission refuses it) never reviews.
    let mut odd = asking();
    odd.access.autonomous = true;
    let v = verdict(&odd, false, &[], &call(write(&format!("{ACME}/a"))));
    assert!(matches!(v, Verdict::Ask { .. }), "{v:?}");
}

#[test]
fn rule_9_otherwise_asks_with_every_choice_that_fits() {
    assert_eq!(
        verdict(
            &asking(),
            false,
            &[],
            &in_folder(write(&format!("{ACME}/a")), ACME)
        ),
        asks(Some(Always::Folder(p(ACME))), Why::InOwnFolder)
    );
    assert_eq!(
        verdict(
            &asking(),
            false,
            &[],
            &connector("linear.issue_create", "linear", true)
        ),
        asks(
            Some(Always::Connector("linear".into())),
            Why::ConnectorWrite
        )
    );
    // Without a session, no "for this session".
    let sessionless = CallFacts {
        session_id: None,
        ..in_folder(write(&format!("{ACME}/a")), ACME)
    };
    assert_eq!(
        verdict(&asking(), false, &[], &sessionless),
        Verdict::Ask {
            choices: vec![
                Choice::AllowOnce,
                Choice::Always(Always::Folder(p(ACME))),
                Choice::Deny
            ],
            why: Why::InOwnFolder
        }
    );
}

#[test]
fn always_is_never_offered_for_home_the_root_a_volume_or_a_folder_not_holding_the_target() {
    let mut everywhere = asking();
    everywhere.own = vec![p("/")];
    for folder in [
        "/Users/sarah",
        "/",
        "/Users",
        "/Volumes/Backup",
        "/Users/sarah/dev/other",
    ] {
        let target = if folder == "/Volumes/Backup" {
            "/Volumes/Backup/a"
        } else {
            "/Users/sarah/dev/acme-web/a"
        };
        assert_eq!(
            verdict(&everywhere, false, &[], &in_folder(write(target), folder)),
            asks(None, Why::InOwnFolder),
            "{folder}"
        );
    }
    assert_eq!(
        verdict(
            &everywhere,
            false,
            &[],
            &in_folder(write("/Volumes/Backup/proj/a"), "/Volumes/Backup/proj")
        ),
        asks(
            Some(Always::Folder(p("/Volumes/Backup/proj"))),
            Why::InOwnFolder
        )
    );
}

#[test]
fn a_change_that_names_no_path_is_not_taken_as_inside() {
    for effect in [
        Effect::Write { paths: vec![] },
        Effect::Delete { paths: vec![] },
    ] {
        assert_eq!(verdict(&full(), false, &[], &call(effect.clone())), run());
        assert_eq!(
            verdict(&asking(), false, &[], &call(effect)),
            asks(None, Why::OutsideFolder)
        );
    }
}

#[test]
fn a_folder_grant_matches_by_whole_path_components() {
    let v = verdict(
        &asking(),
        false,
        &[grant(Ability::Write, "/Users/sarah/dev/acme")],
        &call(write("/Users/sarah/dev/acme-web/a")),
    );
    assert!(matches!(v, Verdict::Ask { .. }));
    let v = verdict(
        &policy(Access::ReadAnywhere),
        false,
        &[],
        &call(write("/Users/sarah/Desk/hub/agents/allan2/a")),
    );
    assert_eq!(refused(&v).map(|r| r.0), Some(Refused::OutsideLevel));
}

#[test]
fn namespaces_classify_tools_by_prefix() {
    for tool in [
        "filesystem.read",
        "exec.run",
        "code.edit",
        "desk.start",
        "coding.session",
        "attachments.save",
    ] {
        assert_eq!(namespace(tool), Some(Namespace::Computer), "{tool}");
    }
    for tool in ["linear.issue_create", "slack.post_message", "github.pr_get"] {
        assert_eq!(namespace(tool), Some(Namespace::Connector), "{tool}");
    }
    for tool in [
        "memory.recall",
        "web.fetch",
        "artifact.create",
        "messaging.send",
        "knowledge.search",
        "files",
    ] {
        assert_eq!(namespace(tool), None, "{tool}");
    }
    assert!(sends_to_people("slack.post_message"));
    assert!(sends_to_people("email.forward"));
    assert!(sends_to_people("postmark.email_send_batch"));
    assert!(!sends_to_people("slack.channels_list"));
    assert!(!sends_to_people("email.draft"));
    // Every tool Desk's connectors send to people with.
    for send in [
        "slack.send",
        "slack.post_message",
        "email.send",
        "email.forward",
        "google.gmail.send",
        "google.gmail.draft_send",
        "google.chat.messages_send",
        "google.chat.dm_send",
        "sendgrid.email_send",
    ] {
        assert!(sends_to_people(send), "{send} sends to people");
    }
    for not in [
        "slack.read",
        "slack.react",
        "google.gmail.search",
        "google.gmail.draft_create",
    ] {
        assert!(!sends_to_people(not), "{not} sends to nobody");
    }
}

#[test]
fn paths_are_shown_under_home_with_a_tilde() {
    let home = Path::new("/Users/sarah");
    assert_eq!(shown(Path::new("/Users/sarah"), home), "~");
    assert_eq!(shown(Path::new("/Users/sarah/dev"), home), "~/dev");
    assert_eq!(shown(Path::new("/etc/hosts"), home), "/etc/hosts");
}

/// Protected names are compared as a case-insensitive disk compares them:
/// a different case, a long s or a ligature names the same file.
#[test]
fn the_withheld_set_and_config_are_compared_as_the_disk_compares_names() {
    for effect in [
        read("/users/SARAH/library/application support/desk/vault.db"),
        read("/Users/\u{17f}arah/Library/Keychains/login.keychain-db"),
        write(&format!("{OWN}/CONFIG.TOML")),
        write(&format!("{OWN}/soul.md")),
    ] {
        let v = verdict(&full(), false, &[], &call(effect.clone()));
        assert_eq!(
            refused(&v).map(|r| r.0),
            Some(Refused::Withheld),
            "{effect:?}"
        );
    }
}

/// At Full access, writing or moving onto a file outside the folder that is
/// there already asks; a new file there doesn't.
#[test]
fn writing_over_a_file_outside_the_folder_asks_at_full_access() {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(base.join("own")).unwrap();
    std::fs::create_dir_all(base.join("elsewhere")).unwrap();
    std::fs::write(base.join("elsewhere/there.txt"), "x").unwrap();
    let mut policy = full();
    policy.own = vec![base.join("own")];
    policy.scratch = vec![];
    let there = base.join("elsewhere/there.txt").display().to_string();
    let new = base.join("elsewhere/new.txt").display().to_string();
    let v = verdict(&policy, false, &[], &call(write(&there)));
    assert!(matches!(v, Verdict::Ask { .. }), "{v:?}");
    let v = verdict(&policy, false, &[], &call(write(&new)));
    assert!(matches!(v, Verdict::Run { .. }), "{v:?}");
    let moved = Effect::Move {
        from: vec![base.join("own/a.txt")],
        to: PathBuf::from(&there),
    };
    let v = verdict(&policy, false, &[], &call(moved));
    assert!(matches!(v, Verdict::Ask { .. }), "{v:?}");
    let v = verdict(
        &policy,
        false,
        &[],
        &call(write(&base.join("own/a.txt").display().to_string())),
    );
    assert!(matches!(v, Verdict::Run { .. }), "{v:?}");
}

// ── The browser ──────────────────────────────────────────────────────────

/// Tools that only read a page, and tools that change one.
const BROWSER_READS: &[&str] = &[
    "browser.navigate",
    "browser.snapshot",
    "browser.screenshot",
    "browser.pdf",
    "browser.get_content",
    "browser.console_messages",
    "browser.network_requests",
    "browser.wait",
];
const BROWSER_WRITES: &[&str] = &[
    "browser.click",
    "browser.fill",
    "browser.type",
    "browser.press_key",
    "browser.select_option",
    "browser.evaluate",
    "browser.upload_file",
];

fn on_page(tool: &str, write: bool) -> CallFacts {
    CallFacts {
        tool: tool.into(),
        ..call(Effect::Connector {
            connector: browser_site(Some("shop.example")),
            write,
        })
    }
}

fn browsing_as(browsing: Browsing, access: Access, hostile: bool) -> AgentPolicy {
    let mut p_ = policy(access);
    p_.browser = Browser { browsing, hostile };
    p_
}

fn page_ask(session: bool) -> Verdict {
    let mut choices = vec![Choice::AllowOnce];
    if session {
        choices.push(Choice::AllowSession);
    }
    choices.push(Choice::Deny);
    Verdict::Ask {
        choices,
        why: Why::OnAPage,
    }
}

/// The verdict table: each standing (unbound, an overseer at Act short of
/// unbound and every other agent, one that only suggests) by each tool.
#[test]
fn browser_verdicts_by_standing_and_tool() {
    let run = Verdict::Run { by: By::Rule };
    for access in [Access::Full, Access::ReadAnywhere, Access::OwnFolder] {
        for tool in BROWSER_READS {
            for browsing in [Browsing::Unbound, Browsing::Asks, Browsing::Suggests] {
                for hostile in [false, true] {
                    assert_eq!(
                        verdict(
                            &browsing_as(browsing, access, hostile),
                            false,
                            &[],
                            &on_page(tool, false)
                        ),
                        run,
                        "{tool} reads at {browsing:?} {access:?} hostile={hostile}"
                    );
                }
            }
        }
        for tool in BROWSER_WRITES {
            let write = on_page(tool, true);
            assert_eq!(
                verdict(
                    &browsing_as(Browsing::Unbound, access, false),
                    false,
                    &[],
                    &write
                ),
                run,
                "unbound {tool}"
            );
            assert_eq!(
                verdict(
                    &browsing_as(Browsing::Asks, access, false),
                    false,
                    &[],
                    &write
                ),
                page_ask(true),
                "asks {tool} at {access:?}"
            );
            let suggests = verdict(
                &browsing_as(Browsing::Suggests, access, false),
                false,
                &[],
                &write,
            );
            assert!(
                matches!(&suggests, Verdict::Refuse { refusal } if refusal.rule == Refused::OutsideLevel && refusal.sentence.contains("only suggests")),
                "suggests {tool}: {suggests:?}"
            );
        }
    }
}

/// Neither the computer's kill switch nor the agent's level holds the
/// browser: it has its own off switch in Desk's settings.
#[test]
fn the_computer_kill_switch_and_level_leave_the_browser_to_its_own_rules() {
    let p_ = browsing_as(Browsing::Unbound, Access::OwnFolder, false);
    assert_eq!(
        verdict(&p_, true, &[], &on_page("browser.click", true)),
        Verdict::Run { by: By::Rule }
    );
}

/// A hostile page puts even an unbound overseer back to asking, once at a
/// time: no allowance, standing or Full access covers it.
#[test]
fn a_hostile_page_asks_every_time_whatever_covers_the_call() {
    let hostile_ask = Verdict::Ask {
        choices: vec![Choice::AllowOnce, Choice::Deny],
        why: Why::HostilePage,
    };
    let allowance = site_allowance("shop.example", "s1");
    for browsing in [Browsing::Unbound, Browsing::Asks] {
        let mut p_ = browsing_as(browsing, Access::Full, true);
        p_.access.asks = Asking::Never;
        assert_eq!(
            verdict(
                &p_,
                false,
                std::slice::from_ref(&allowance),
                &on_page("browser.click", true)
            ),
            hostile_ask,
            "{browsing:?}"
        );
    }
}

fn site_allowance(host: &str, session: &str) -> Grant {
    Grant {
        id: Uuid::from_u128(7),
        hub: "local".into(),
        agent: "allan".into(),
        person: "sarah".into(),
        ability: Ability::ConnectorWrite,
        folder: None,
        connector: Some(browser_site(Some(host))),
        session_id: Some(session.into()),
        run: None,
    }
}

/// "Allow for this session" holds for its site and its session only.
#[test]
fn a_session_allowance_covers_its_site_in_its_session() {
    let p_ = browsing_as(Browsing::Asks, Access::Full, false);
    let click = on_page("browser.click", true);
    assert_eq!(
        verdict(&p_, false, &[site_allowance("shop.example", "s1")], &click),
        Verdict::Run {
            by: By::Session(Uuid::from_u128(7))
        }
    );
    assert_eq!(
        verdict(&p_, false, &[site_allowance("other.example", "s1")], &click),
        page_ask(true),
        "another site"
    );
    assert_eq!(
        verdict(&p_, false, &[site_allowance("shop.example", "s2")], &click),
        page_ask(true),
        "another session"
    );
}

/// An agent at Full access that never asks ("do anything") changes pages
/// as it does everything else; one that asks is asked.
#[test]
fn full_access_that_never_asks_acts_on_pages() {
    let mut p_ = browsing_as(Browsing::Asks, Access::Full, false);
    p_.access.asks = Asking::Never;
    assert_eq!(
        verdict(&p_, false, &[], &on_page("browser.fill", true)),
        Verdict::Run { by: By::Rule }
    );
}

/// A call with no session offers no session allowance.
#[test]
fn a_browser_ask_outside_a_session_offers_allow_once_only() {
    let p_ = browsing_as(Browsing::Asks, Access::Full, false);
    let click = CallFacts {
        session_id: None,
        ..on_page("browser.click", true)
    };
    assert_eq!(verdict(&p_, false, &[], &click), page_ask(false));
}

#[test]
fn a_browser_site_is_kept_by_its_host() {
    assert_eq!(browser_site(Some("shop.example")), "browser:shop.example");
    assert_eq!(browser_site(None), "browser");
    assert_eq!(browser_site(Some("")), "browser");
}

/// An agent at Own folder given `ACME` to work in.
fn working_in_acme(access: Access) -> AgentPolicy {
    AgentPolicy {
        works_in: vec![p(ACME)],
        ..policy(access)
    }
}

#[test]
fn a_folder_it_works_in_reads_and_changes_as_its_own_at_every_level() {
    for access in [Access::OwnFolder, Access::ReadAnywhere, Access::Full] {
        let pol = working_in_acme(access);
        let file = format!("{ACME}/src/app.ts");
        assert_eq!(
            verdict(&pol, false, &[], &call(read(&file))),
            run(),
            "{access:?}"
        );
        assert_eq!(
            verdict(&pol, false, &[], &call(write(&file))),
            run(),
            "{access:?}"
        );
        // Without it, Own folder reaches none of it.
        let without = policy(access);
        if access == Access::OwnFolder {
            assert_eq!(
                refused(&verdict(&without, false, &[], &call(read(&file)))).map(|r| r.0),
                Some(Refused::OutsideLevel)
            );
        }
    }
}

#[test]
fn a_folder_it_works_in_sits_beside_its_own_never_in_place_of_it() {
    let pol = working_in_acme(Access::OwnFolder);
    assert_eq!(
        verdict(&pol, false, &[], &call(write(&format!("{OWN}/notes.md")))),
        run()
    );
    assert_eq!(
        refused(&verdict(
            &pol,
            false,
            &[],
            &call(read("/Users/sarah/dev/other/a.rs"))
        ))
        .map(|r| r.0),
        Some(Refused::OutsideLevel),
        "a sibling folder stays outside"
    );
    assert_eq!(
        refused(&verdict(
            &pol,
            false,
            &[],
            &call(read("/Users/sarah/dev/acme-web-old/a.rs"))
        ))
        .map(|r| r.0),
        Some(Refused::OutsideLevel),
        "a folder whose name starts the same is not inside"
    );
}

#[test]
fn deleting_in_a_folder_it_works_in_still_asks() {
    let pol = working_in_acme(Access::Full);
    let file = format!("{ACME}/src/old.ts");
    assert!(
        matches!(
            verdict(&pol, false, &[], &call(delete(&file))),
            Verdict::Ask {
                why: Why::CantBeUndone,
                ..
            }
        ),
        "the person's own work: a delete there can't be undone"
    );
    assert!(matches!(
        verdict(&pol, false, &[], &call(exec("rm -rf src", ACME))),
        Verdict::Ask {
            why: Why::CantBeUndone,
            ..
        }
    ));
    // In its own folder a delete runs, as before.
    assert_eq!(
        verdict(
            &pol,
            false,
            &[],
            &call(delete(&format!("{OWN}/scratch.md")))
        ),
        run()
    );
}

#[test]
fn a_folder_it_works_in_never_carves_out_what_desk_withholds() {
    // Even were one given above Desk's data, the withheld set stands.
    let pol = AgentPolicy {
        works_in: vec![p("/Users/sarah/Library")],
        ..policy(Access::Full)
    };
    assert_eq!(
        refused(&verdict(
            &pol,
            false,
            &[],
            &call(read("/Users/sarah/Library/Keychains/login.keychain-db"))
        ))
        .map(|r| r.0),
        Some(Refused::Withheld)
    );
    assert_eq!(
        refused(&verdict(
            &pol,
            false,
            &[],
            &call(read(&format!("{DATA}/hub/data/vault.db")))
        ))
        .map(|r| r.0),
        Some(Refused::Withheld)
    );
}

#[test]
fn commands_in_a_folder_it_works_in_follow_the_level() {
    // Below Full access commands can't be contained on this computer yet.
    let own = working_in_acme(Access::OwnFolder);
    assert_eq!(
        refused(&verdict(&own, false, &[], &call(exec("npm test", ACME)))).map(|r| r.0),
        Some(Refused::OutsideLevel)
    );
    let full = working_in_acme(Access::Full);
    assert_eq!(
        verdict(&full, false, &[], &call(exec("npm test", ACME))),
        run()
    );
}

#[test]
fn a_session_may_start_in_a_folder_it_works_in() {
    let pol = AgentPolicy {
        contained: true,
        ..working_in_acme(Access::OwnFolder)
    };
    assert_eq!(
        verdict(
            &pol,
            false,
            &[],
            &call(Effect::Session { cwd: Some(p(ACME)) })
        ),
        run()
    );
}
