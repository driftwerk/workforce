//! A board linked to one of a person's several Linear workspaces (or Jira
//! sites) on a hub: the dialog is told which they have, by name, before
//! any team is listed; the link keeps the one picked; and every call its
//! sync makes names it. A link made before keeps today's calls.

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use serde_json::{json, Value};
use workforce_agent::tools::{Tool, ToolResult};
use workforce_postgres::test_support::with_test_pool;

use super::tests::{board, local_hub, LocalHub, RemoteHub};
use crate::remote_mcp::testing::{fixture, with_agents};

const WORKSPACES: [&str; 3] = ["driftwerk", "mantiscrm", "petmobile"];
const MAN_MANTIS: &str = "8a7b6c5d-0000-4000-8000-00000000000a";
const MAN_PET: &str = "8a7b6c5d-0000-4000-8000-00000000000b";

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

/// A fake Linear of three workspaces, as a person's own keys make one: a
/// call naming none of them fails as the adapter's does. Two teams share
/// the key MAN, one in each of two workspaces.
struct FakeLinear {
    name: &'static str,
    calls: Calls,
}

#[async_trait::async_trait]
impl Tool for FakeLinear {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "a fake Linear tool of three workspaces"
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    async fn execute(&self, input: Value) -> eyre::Result<ToolResult> {
        self.calls
            .lock()
            .unwrap()
            .push((self.name.to_string(), input.clone()));
        if self.name == "linear.workspaces" {
            return Ok(ToolResult::success(
                json!({ "workspaces": WORKSPACES, "default": null }),
                0,
            ));
        }
        let asked = input["workspace"].as_str().unwrap_or("default");
        if !WORKSPACES.contains(&asked) {
            return Ok(ToolResult::failure(
                workforce_core::instances::unknown_instance_text(
                    "Linear workspace",
                    asked,
                    &WORKSPACES,
                ),
                0,
            ));
        }
        Ok(ToolResult::success(
            match (self.name, asked) {
                ("linear.teams_list", "mantiscrm") => {
                    json!({ "teams": [{ "id": MAN_MANTIS, "key": "MAN", "name": "Mantis" }] })
                }
                ("linear.teams_list", "petmobile") => {
                    json!({ "teams": [{ "id": MAN_PET, "key": "MAN", "name": "Pet mobile" }] })
                }
                ("linear.teams_list", _) => json!({ "teams": [] }),
                ("linear.states_list", _) => json!({ "states": [
                    { "name": "Todo", "type": "unstarted" },
                    { "name": "In Progress", "type": "started" },
                    { "name": "Done", "type": "completed" },
                ]}),
                _ => json!({ "issues": [] }),
            },
            0,
        ))
    }
}

/// A hub whose Linear is the fake's; `listed`: it offers `linear.workspaces`
/// (an older hub doesn't).
async fn hub_of_workspaces(dir: &std::path::Path, listed: bool) -> (RemoteHub, Calls) {
    let fx = fixture(true);
    with_agents(&fx, dir, &["default"]).await;
    let calls: Calls = Arc::default();
    let service = fx.state.registry.default_agent().unwrap();
    let mut names: Vec<&'static str> = workforce_desk::board_sync::LINEAR_TOOLS.to_vec();
    if listed {
        names.push("linear.workspaces");
    }
    for name in names {
        service.tools.register(Arc::new(FakeLinear {
            name,
            calls: Arc::clone(&calls),
        }));
    }
    let remote = super::tests::serve_remote(fx, &["linear.*"], Arc::default(), None).await;
    (remote, calls)
}

fn check(workspace: Option<&str>) -> Value {
    let mut b = json!({ "system": "linear", "via": { "kind": "hub", "hub": "atlas" } });
    if let Some(w) = workspace {
        b["workspace"] = json!(w);
    }
    b
}

fn link(workspace: Option<&str>, team: &str) -> Value {
    let mut b = json!({
        "system": "linear", "target": "MAN", "targetId": team,
        "via": { "kind": "hub", "hub": "atlas" },
        "filter": { "kind": "all", "value": { "includeDone": false } }, "states": {},
    });
    if let Some(w) = workspace {
        b["workspace"] = json!(w);
    }
    b
}

async fn checked(local: &LocalHub, workspace: Option<&str>) -> Value {
    let (status, body) = local
        .send("POST", "/api/desk/boards/link/check", check(workspace))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

#[tokio::test]
async fn several_linear_workspaces_are_picked_from_and_kept_on_the_link() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let desk = crate::desk_crew::tests::desk(pool, dir.path()).await;
        let (remote, calls) = hub_of_workspaces(&dir.path().join("remote"), true).await;
        let local = local_hub(desk.clone(), dir.path());
        local.link_atlas(&remote).await;

        // Asked with none: their names, plainly, and no teams yet.
        let first = checked(&local, None).await;
        assert_eq!(first["workspaces"], json!(WORKSPACES));
        assert_eq!(
            first["workspaceWhy"],
            "Pick which Linear workspace: driftwerk, mantiscrm or petmobile"
        );
        assert!(first.get("teams").is_none(), "{first}");
        assert!(!first.to_string().contains("'default'"), "{first}");

        // The one picked: its own teams.
        let picked = checked(&local, Some("mantiscrm")).await;
        assert_eq!(picked["teams"][0]["id"], MAN_MANTIS);
        assert_eq!(picked["workspace"], "mantiscrm");

        // One that isn't theirs: said plainly, nothing listed.
        let wrong = checked(&local, Some("globex")).await;
        assert_eq!(
            wrong["workspaceWhy"],
            "There's no Linear workspace globex of yours there now. Pick which Linear workspace: driftwerk, mantiscrm or petmobile"
        );
        assert!(wrong.get("teams").is_none());

        // A link with none picked is refused in the same words.
        let b = board(&desk, "Mantis").await;
        let uri = format!("/api/desk/boards/{}/link", b.id);
        let (status, said) = local.send("PUT", &uri, link(None, MAN_MANTIS)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{said}");
        assert_eq!(
            said["error"],
            "Pick which Linear workspace: driftwerk, mantiscrm or petmobile"
        );

        // Picked, it is kept, and every call of the sync names it.
        let (status, linked) = local
            .send("PUT", &uri, link(Some("mantiscrm"), MAN_MANTIS))
            .await;
        assert_eq!(status, StatusCode::OK, "{linked}");
        assert_eq!(linked["sync"]["workspace"], "mantiscrm");
        let before = calls.lock().unwrap().len();
        local.sync.pass_board(b.id, true).await.unwrap();
        let after: Vec<(String, Value)> = calls.lock().unwrap()[before..].to_vec();
        assert!(!after.is_empty(), "the pass called Linear");
        for (tool, args) in &after {
            assert_eq!(args["workspace"], "mantiscrm", "{tool} {args}");
        }
        let (_, s) = desk.board_sync(b.id).await.unwrap().unwrap();
        assert!(s.paused.is_none(), "{:?}", s.paused);

        // The same team key in another workspace links to another board.
        let pet = board(&desk, "Pet mobile").await;
        let (status, said) = local
            .send(
                "PUT",
                &format!("/api/desk/boards/{}/link", pet.id),
                link(Some("petmobile"), MAN_PET),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{said}");
    })
    .await;
}

/// A hub not yet updated, without `linear.workspaces`: the names come from
/// the error a call naming none gets, said plainly.
#[tokio::test]
async fn an_older_hub_s_workspaces_come_from_its_error() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let desk = crate::desk_crew::tests::desk(pool, dir.path()).await;
        let (remote, _) = hub_of_workspaces(&dir.path().join("remote"), false).await;
        let local = local_hub(desk.clone(), dir.path());
        local.link_atlas(&remote).await;
        let first = checked(&local, None).await;
        assert_eq!(first["workspaces"], json!(WORKSPACES));
        assert_eq!(
            first["workspaceWhy"],
            "Pick which Linear workspace: driftwerk, mantiscrm or petmobile"
        );
        let picked = checked(&local, Some("mantiscrm")).await;
        assert_eq!(picked["teams"][0]["id"], MAN_MANTIS);
        let b = board(&desk, "Mantis").await;
        let (status, linked) = local
            .send(
                "PUT",
                &format!("/api/desk/boards/{}/link", b.id),
                link(Some("mantiscrm"), MAN_MANTIS),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{linked}");
        assert_eq!(linked["sync"]["workspace"], "mantiscrm");
    })
    .await;
}

/// A link made before workspaces were kept, on a hub where the person now
/// has several: its calls still name none, and the board pauses saying
/// why, never "default".
#[tokio::test]
async fn a_link_without_a_workspace_pauses_plainly_when_there_are_several() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let desk = crate::desk_crew::tests::desk(pool, dir.path()).await;
        let (remote, calls) = hub_of_workspaces(&dir.path().join("remote"), false).await;
        let local = local_hub(desk.clone(), dir.path());
        local.link_atlas(&remote).await;
        let b = board(&desk, "Mantis").await;
        let (status, linked) = local
            .send(
                "PUT",
                &format!("/api/desk/boards/{}/link", b.id),
                link(Some("mantiscrm"), MAN_MANTIS),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{linked}");
        // As a link made before was kept: no workspace.
        desk.change_sync(b.id, |s| s.workspace = None)
            .await
            .unwrap();
        let before = calls.lock().unwrap().len();
        local.sync.pass_board(b.id, true).await.unwrap();
        for (tool, args) in &calls.lock().unwrap()[before..] {
            assert!(args.get("workspace").is_none(), "{tool} {args}");
        }
        let (_, s) = desk.board_sync(b.id).await.unwrap().unwrap();
        let paused = s.paused.expect("paused");
        let words = paused.say("Driftwerk");
        assert_eq!(
            words,
            "Paused: you have more than one Linear workspace on Driftwerk (driftwerk, mantiscrm or petmobile), and this link doesn't say which."
        );
        assert!(!s.problem.unwrap_or_default().contains("'default'"));
    })
    .await;
}

const SITES: [&str; 3] = ["acme", "client", "globex"];

/// A fake Jira of three sites, no default. `older`: a hub not yet updated,
/// without `jira.workspaces`, whose Jira says a missing default its own way.
struct FakeJira {
    name: &'static str,
    older: bool,
}

#[async_trait::async_trait]
impl Tool for FakeJira {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "a fake Jira tool of three sites"
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    async fn execute(&self, input: Value) -> eyre::Result<ToolResult> {
        if self.name == "jira.workspaces" {
            return Ok(ToolResult::success(
                json!({ "workspaces": SITES, "default": null }),
                0,
            ));
        }
        let asked = input["workspace"].as_str();
        let Some(site) = asked.filter(|a| SITES.contains(a)) else {
            let said = match (self.older, asked) {
                (true, None) => {
                    format!("Default Jira workspace 'default' not configured. Available: {SITES:?}")
                }
                (_, a) => workforce_core::instances::unknown_instance_text(
                    "Jira workspace",
                    a.unwrap_or("default"),
                    &SITES,
                ),
            };
            return Ok(ToolResult::failure(said, 0));
        };
        Ok(ToolResult::success(
            match self.name {
                "jira.list_projects" => {
                    json!({ "projects": [{ "key": site.to_ascii_uppercase(), "name": site }] })
                }
                _ => json!({ "issues": [] }),
            },
            0,
        ))
    }
}

async fn hub_of_sites(dir: &std::path::Path, older: bool) -> RemoteHub {
    let fx = fixture(true);
    with_agents(&fx, dir, &["default"]).await;
    let service = fx.state.registry.default_agent().unwrap();
    let mut names: Vec<&'static str> = workforce_desk::board_sync::JIRA_TOOLS.to_vec();
    names.push("jira.list_projects");
    if !older {
        names.push("jira.workspaces");
    }
    for name in names {
        service.tools.register(Arc::new(FakeJira { name, older }));
    }
    super::tests::serve_remote(fx, &["jira.*"], Arc::default(), None).await
}

/// Jira sites as Linear workspaces: listed (or read from an older hub's
/// own wording), picked, their projects listed, and the pick kept.
#[tokio::test]
async fn several_jira_sites_are_picked_from_too() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        // One database for both: each links its own site's project.
        for (older, site) in [(false, "client"), (true, "globex")] {
            let dir = tempfile::tempdir().unwrap();
            let desk = crate::desk_crew::tests::desk(pool.clone(), dir.path()).await;
            let remote = hub_of_sites(&dir.path().join("remote"), older).await;
            let local = local_hub(desk.clone(), dir.path());
            local.link_atlas(&remote).await;
            let ask = |w: Option<&str>| {
                let mut b = json!({ "system": "jira", "via": { "kind": "hub", "hub": "atlas" } });
                if let Some(w) = w {
                    b["workspace"] = json!(w);
                }
                b
            };
            let (status, first) = local
                .send("POST", "/api/desk/boards/link/check", ask(None))
                .await;
            assert_eq!(status, StatusCode::OK, "{first}");
            assert_eq!(first["workspaces"], json!(SITES), "older {older}");
            assert_eq!(
                first["workspaceWhy"],
                "Pick which Jira site: acme, client or globex"
            );
            assert!(!first.to_string().contains("'default'"), "{first}");
            let (_, picked) = local
                .send("POST", "/api/desk/boards/link/check", ask(Some(site)))
                .await;
            let key = site.to_ascii_uppercase();
            assert_eq!(picked["projects"][0]["key"], key.as_str(), "{picked}");
            let b = board(&desk, &format!("Client {older}")).await;
            let (status, linked) = local
                .send(
                    "PUT",
                    &format!("/api/desk/boards/{}/link", b.id),
                    json!({
                        "system": "jira", "target": key, "workspace": site,
                        "via": { "kind": "hub", "hub": "atlas" },
                        "filter": { "kind": "all", "value": { "includeDone": false } }, "states": {},
                    }),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{linked}");
            assert_eq!(linked["sync"]["workspace"], site);
        }
    })
    .await;
}

const CONNECTIONS: [&str; 3] = ["driftwerk", "mantis", "patchwork"];

/// A fake GitHub of three connections, `mantis` the default, as a person's
/// own keys make one: only the connections in `reach` reach acme's
/// project 7.
struct FakeGithub {
    name: &'static str,
    reach: &'static [&'static str],
    calls: Calls,
}

#[async_trait::async_trait]
impl Tool for FakeGithub {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "a fake GitHub tool of three connections"
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    async fn execute(&self, input: Value) -> eyre::Result<ToolResult> {
        self.calls
            .lock()
            .unwrap()
            .push((self.name.to_string(), input.clone()));
        if self.name == "github.workspaces" {
            return Ok(ToolResult::success(
                json!({ "workspaces": CONNECTIONS, "default": "mantis" }),
                0,
            ));
        }
        let through = input["workspace"].as_str().unwrap_or("mantis");
        if !self.reach.contains(&through) {
            return Ok(ToolResult::failure("Not found".to_string(), 0));
        }
        Ok(ToolResult::success(
            match self.name {
                "github.project_get" => json!({
                    "id": "PVT_acme7", "title": "Acme", "number": 7,
                    "status": { "options": [{ "name": "Todo" }, { "name": "In Progress" }, { "name": "Done" }] },
                }),
                _ => json!({ "items": [], "issues": [] }),
            },
            0,
        ))
    }
}

/// A hub whose GitHub is the fake's.
async fn hub_of_connections(
    dir: &std::path::Path,
    reach: &'static [&'static str],
) -> (RemoteHub, Calls) {
    let fx = fixture(true);
    with_agents(&fx, dir, &["default"]).await;
    let calls: Calls = Arc::default();
    let service = fx.state.registry.default_agent().unwrap();
    for name in workforce_desk::github_sync::TOOLS
        .iter()
        .chain(&["github.workspaces", "github.viewer"])
    {
        service.tools.register(Arc::new(FakeGithub {
            name,
            reach,
            calls: Arc::clone(&calls),
        }));
    }
    let remote = super::tests::serve_remote(fx, &["github.*"], Arc::default(), None).await;
    (remote, calls)
}

fn project(workspace: Option<&str>, link: bool) -> Value {
    let mut b = json!({
        "system": "github", "target": "acme/projects/7",
        "via": { "kind": "hub", "hub": "atlas" },
    });
    if link {
        b["filter"] = json!({ "kind": "all", "value": { "includeDone": false } });
        b["states"] = json!({});
    }
    if let Some(w) = workspace {
        b["workspace"] = json!(w);
    }
    b
}

/// Of several GitHub connections, a link goes through the one that reaches
/// its project, though the default doesn't: the check says which, the link
/// keeps it, and every call of the sync names it.
#[tokio::test]
async fn a_github_link_goes_through_the_connection_that_reaches_its_project() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let desk = crate::desk_crew::tests::desk(pool, dir.path()).await;
        let (remote, calls) = hub_of_connections(&dir.path().join("remote"), &["driftwerk"]).await;
        let local = local_hub(desk.clone(), dir.path());
        local.link_atlas(&remote).await;

        for asked in [None, Some("mantis")] {
            let (status, found) = local
                .send("POST", "/api/desk/boards/link/check", project(asked, false))
                .await;
            assert_eq!(status, StatusCode::OK, "{found}");
            assert_eq!(found["workspaces"], json!(CONNECTIONS));
            assert_eq!(found["workspace"], "driftwerk", "{found}");
            assert_eq!(found["targetId"], "PVT_acme7");
        }

        let b = board(&desk, "Acme").await;
        let (status, linked) = local
            .send(
                "PUT",
                &format!("/api/desk/boards/{}/link", b.id),
                project(None, true),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{linked}");
        assert_eq!(linked["sync"]["workspace"], "driftwerk");
        let before = calls.lock().unwrap().len();
        local.sync.pass_board(b.id, true).await.unwrap();
        let after: Vec<(String, Value)> = calls.lock().unwrap()[before..].to_vec();
        assert!(!after.is_empty(), "the pass called GitHub");
        for (tool, args) in after.iter().filter(|(t, _)| t != "github.workspaces") {
            assert_eq!(args["workspace"], "driftwerk", "{tool} {args}");
        }
    })
    .await;
}

/// Two connections reach the project and the default doesn't: the person
/// picks, and the one picked is kept.
#[tokio::test]
async fn two_github_connections_reaching_a_project_are_picked_from() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let desk = crate::desk_crew::tests::desk(pool, dir.path()).await;
        let (remote, _) =
            hub_of_connections(&dir.path().join("remote"), &["driftwerk", "patchwork"]).await;
        let local = local_hub(desk.clone(), dir.path());
        local.link_atlas(&remote).await;
        let why = "Pick which GitHub connection: driftwerk or patchwork";

        let (status, found) = local
            .send("POST", "/api/desk/boards/link/check", project(None, false))
            .await;
        assert_eq!(status, StatusCode::OK, "{found}");
        assert_eq!(found["workspaceWhy"], why);
        assert_eq!(found["workspaces"], json!(CONNECTIONS));
        assert!(found.get("states").is_none(), "{found}");

        let b = board(&desk, "Acme").await;
        let uri = format!("/api/desk/boards/{}/link", b.id);
        let (status, said) = local.send("PUT", &uri, project(None, true)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{said}");
        assert_eq!(said["error"], why);

        let (status, linked) = local
            .send("PUT", &uri, project(Some("patchwork"), true))
            .await;
        assert_eq!(status, StatusCode::OK, "{linked}");
        assert_eq!(linked["sync"]["workspace"], "patchwork");
    })
    .await;
}
