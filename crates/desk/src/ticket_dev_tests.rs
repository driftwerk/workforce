//! Reading a ticket's development from what Jira's and Linear's tools say.

use chrono::{TimeZone, Utc};
use serde_json::json;

use super::*;

/// Someone else's words, as a hub wraps them.
fn w(s: &str) -> String {
    format!(
        "<<<EXTERNAL_UNTRUSTED_CONTENT_ab12>>>\n[External content from jira.dev_status]\n{s}\n<<</EXTERNAL_UNTRUSTED_CONTENT_ab12>>>"
    )
}

fn dev_answer() -> Value {
    json!({
        "issue_key": "ACME-1", "issue_id": "10001",
        "prs": [
            { "id": "#12", "number": 12, "url": "https://github.com/acme/app/pull/12", "status": "OPEN",
              "title": w("ACME-1 Retry the upload"), "author": w("Ada Example"), "repo": w("acme/app"),
              "source_branch": w("acme-1-retry"), "last_update": "2026-11-12T22:43:26.000+0000",
              "reviewers": 0, "approvals": 0 },
            { "id": "#9", "number": 9, "url": "https://github.com/acme/app/pull/9", "status": "OPEN",
              "title": w("ACME-1 Lint"), "author": null, "repo": w("acme/app"),
              "source_branch": w("acme-1-lint"), "reviewers": 2, "approvals": 1 },
            { "id": "#8", "url": "https://github.com/acme/app/pull/8", "status": "MERGED",
              "title": w("ACME-1 First go"), "source_branch": w("acme-1-first") },
            { "id": "#7", "url": "http://github.com/acme/app/pull/7", "status": "DECLINED",
              "title": w("Over plain http") },
            { "id": "#6", "url": "javascript:alert(1)", "status": "OPEN", "title": w("Not a link") }
        ],
        "branches": [
            { "name": w("acme-1-retry"), "url": "https://github.com/acme/app/tree/acme-1-retry", "repo": w("acme/app") },
            { "name": w("acme-1-local"), "url": "http://git.example/acme-1-local" },
            { "name": null, "url": "https://github.com/acme/app/tree/x" }
        ],
        "commits": { "count": 3, "latest": "2026-11-12T13:32:28.000+0000" },
        "builds": [
            { "name": w("Build"), "pipeline": "1", "state": "successful", "branch": w("acme-1-retry"), "updated": "2026-11-12T22:51:03Z" },
            { "name": w("Lint"), "pipeline": "2", "state": "cancelled", "branch": w("acme-1-retry"), "updated": "2026-11-12T22:52:00Z" },
            { "name": w("Lint"), "pipeline": "2", "state": "successful", "branch": w("acme-1-retry"), "updated": "2026-11-12T22:40:00Z" },
            { "name": w("Build"), "pipeline": "1", "state": "failed", "branch": w("acme-1-retry"), "updated": "2026-10-01T12:20:13Z" },
            { "name": w("Build"), "pipeline": "1", "state": "in_progress", "branch": w("acme-1-lint"), "updated": "2026-11-12T22:51:03Z" },
            { "name": w("Build"), "pipeline": "1", "state": "failed", "branch": w("acme-1-first"), "updated": "2026-11-01T10:00:00Z" }
        ]
    })
}

#[test]
fn a_jira_issue_s_prs_come_unwrapped_with_their_state_and_only_https() {
    let work = jira_work(&dev_answer()).unwrap();
    let urls: Vec<&str> = work.prs.iter().map(|p| p.url.as_str()).collect();
    assert_eq!(
        urls,
        [
            "https://github.com/acme/app/pull/12",
            "https://github.com/acme/app/pull/9",
            "https://github.com/acme/app/pull/8",
        ],
        "a PR whose address isn't https is dropped"
    );
    let first = &work.prs[0];
    assert_eq!(first.number, Some(12));
    assert_eq!(first.title, "ACME-1 Retry the upload");
    assert_eq!(first.state, "open");
    assert_eq!(first.author.as_deref(), Some("Ada Example"));
    assert_eq!(first.repo.as_deref(), Some("acme/app"));
    assert_eq!(
        first.updated,
        Some(Utc.with_ymd_and_hms(2026, 11, 12, 22, 43, 26).unwrap())
    );
    assert_eq!(
        work.prs[1].state, "review",
        "an open PR with reviewers is in review"
    );
    assert_eq!(work.prs[1].author, None);
    assert_eq!(work.prs[2].state, "merged");
    assert_eq!(work.prs[2].number, Some(8), "its number from its address");
}

#[test]
fn a_pr_s_checks_are_the_latest_run_of_each_pipeline_on_its_branch() {
    let work = jira_work(&dev_answer()).unwrap();
    // Build passed last; Lint's last run was cancelled, so its run before.
    assert_eq!(
        work.prs[0].checks,
        Some(Checks {
            passed: 2,
            total: 2,
            state: "pass".into()
        })
    );
    assert_eq!(
        work.prs[1].checks,
        Some(Checks {
            passed: 0,
            total: 1,
            state: "run".into()
        })
    );
    assert_eq!(
        work.prs[2].checks,
        Some(Checks {
            passed: 0,
            total: 1,
            state: "fail".into()
        })
    );
    // With no build on its branch, no checks: never a guess.
    let mut answer = dev_answer();
    answer["builds"] = json!([]);
    assert!(jira_work(&answer)
        .unwrap()
        .prs
        .iter()
        .all(|p| p.checks.is_none()));
}

#[test]
fn a_jira_issue_s_branches_and_commits_are_kept_and_a_bad_address_is_left_off() {
    let work = jira_work(&dev_answer()).unwrap();
    assert_eq!(
        work.branches,
        vec![
            DevBranch {
                name: "acme-1-retry".into(),
                url: Some("https://github.com/acme/app/tree/acme-1-retry".into()),
                repo: Some("acme/app".into()),
            },
            DevBranch {
                name: "acme-1-local".into(),
                url: None,
                repo: None,
            },
        ],
        "an http address is dropped, a branch with no name isn't listed"
    );
    assert_eq!(
        work.commits,
        Some(Commits {
            count: 3,
            latest: Some(Utc.with_ymd_and_hms(2026, 11, 12, 13, 32, 28).unwrap()),
        })
    );
    assert_eq!(jira_work(&json!({ "error": "nope" })), None);
    assert_eq!(jira_work(&json!("text")), None);
}

#[test]
fn a_jira_issue_s_links_say_their_app_or_host_and_only_https() {
    let answer = json!({ "issue_key": "ACME-1", "links": [
        { "url": "https://acme.atlassian.net/wiki/x/1", "title": w("Upload spec"),
          "app": w("Confluence"), "relationship": w("mentioned in") },
        { "url": "https://www.figma.com/file/abc", "title": null },
        { "url": "http://intranet.example/page", "title": w("Plain http") },
        { "url": "file:///etc/passwd", "title": w("A file") }
    ] });
    assert_eq!(
        jira_links(&answer).unwrap(),
        vec![
            RemoteLink {
                title: "Upload spec".into(),
                url: "https://acme.atlassian.net/wiki/x/1".into(),
                app: Some("Confluence".into()),
                words: Some("mentioned in".into()),
            },
            RemoteLink {
                title: "https://www.figma.com/file/abc".into(),
                url: "https://www.figma.com/file/abc".into(),
                app: Some("figma.com".into()),
                words: None,
            },
        ]
    );
    assert_eq!(jira_links(&json!({})), None);
}

#[test]
fn a_linear_issue_s_pr_attachments_are_prs_and_its_other_links_are_links() {
    let issue = json!({ "identifier": "ATL-7", "attachments": { "nodes": [
        { "id": "a1", "url": "https://github.com/acme/app/pull/31", "sourceType": "github",
          "title": "Retry the upload", "metadata": { "status": "merged", "number": 31,
          "title": "ATL-7 Retry the upload", "repoLogin": "acme", "repoName": "app",
          "userLogin": "ada", "updatedAt": "2026-11-10T09:00:00.000Z" } },
        { "id": "a2", "url": "https://gitlab.com/acme/app/-/merge_requests/4", "sourceType": "gitlab",
          "title": "Draft it", "metadata": { "status": "opened", "draft": true } },
        { "id": "a3", "url": "https://www.figma.com/file/abc", "sourceType": "figma", "title": "Retry screen" },
        { "id": "a4", "url": "https://docs.example/page", "sourceType": "api", "title": null },
        { "id": "a5", "url": "https://uploads.linear.app/a/b/log.txt", "title": "log.txt" },
        { "id": "a6", "url": "http://insecure.example/x", "title": "Plain http" }
    ] } });
    let (work, links) = linear_dev(&issue).unwrap();
    assert_eq!(
        work.prs,
        vec![
            DevPr {
                number: Some(31),
                url: "https://github.com/acme/app/pull/31".into(),
                title: "ATL-7 Retry the upload".into(),
                state: "merged".into(),
                checks: None,
                author: Some("ada".into()),
                repo: Some("acme/app".into()),
                updated: Some(Utc.with_ymd_and_hms(2026, 11, 10, 9, 0, 0).unwrap()),
            },
            DevPr {
                number: Some(4),
                url: "https://gitlab.com/acme/app/-/merge_requests/4".into(),
                title: "Draft it".into(),
                state: "draft".into(),
                checks: None,
                author: None,
                repo: None,
                updated: None,
            },
        ]
    );
    assert!(work.branches.is_empty() && work.commits.is_none());
    assert_eq!(
        links,
        vec![
            RemoteLink {
                title: "Retry screen".into(),
                url: "https://www.figma.com/file/abc".into(),
                app: Some("Figma".into()),
                words: None,
            },
            RemoteLink {
                title: "https://docs.example/page".into(),
                url: "https://docs.example/page".into(),
                app: Some("docs.example".into()),
                words: None,
            },
        ],
        "an upload is a file, not a link; an http one is dropped"
    );
    assert_eq!(linear_dev(&json!({ "identifier": "ATL-7" })), None);
}

#[test]
fn a_part_that_wasn_t_read_is_left_as_it_was() {
    let then = Utc.with_ymd_and_hms(2026, 11, 1, 9, 0, 0).unwrap();
    let now = Utc.with_ymd_and_hms(2026, 11, 13, 9, 0, 0).unwrap();
    // Nothing read, nothing known: still absent.
    assert_eq!(merged(None, None, None, now), None);
    let link = RemoteLink {
        title: "Spec".into(),
        url: "https://acme.example/spec".into(),
        app: None,
        words: None,
    };
    let was = merged(
        None,
        Some(jira_work(&dev_answer()).unwrap()),
        Some(vec![link.clone()]),
        then,
    )
    .unwrap();
    assert_eq!(was.prs.len(), 3);
    assert_eq!(was.read_at, then);
    // Nothing read now: left whole, with when it was read.
    assert_eq!(
        merged(Some(was.clone()), None, None, now),
        Some(was.clone())
    );
    // Only the links read: the PRs stay.
    let got = merged(Some(was.clone()), None, Some(vec![]), now).unwrap();
    assert_eq!(got.prs, was.prs);
    assert_eq!(got.commits, was.commits);
    assert!(got.links.is_empty());
    assert_eq!(got.read_at, now);
    // Only the work read: the links stay.
    let got = merged(Some(was.clone()), Some(Work::default()), None, now).unwrap();
    assert!(got.prs.is_empty() && got.branches.is_empty() && got.commits.is_none());
    assert_eq!(got.links, vec![link]);
}

#[test]
fn a_dev_status_call_names_the_issue_s_id_only_when_it_is_one() {
    let mut e: External = serde_json::from_value(json!({
        "system": "jira", "key": "ACME-1", "id": "10001", "url": "https://acme.example/browse/ACME-1",
        "remoteUpdatedAt": "2026-11-12T00:00:00Z", "synced": true, "link": uuid::Uuid::nil(),
    }))
    .unwrap();
    assert_eq!(
        dev_status_call(&e),
        (
            "jira.dev_status",
            json!({ "issue_key": "ACME-1", "issue_id": "10001" })
        )
    );
    e.id = Some("1&x=2".into());
    assert_eq!(dev_status_call(&e).1, json!({ "issue_key": "ACME-1" }));
    assert_eq!(
        remote_links_call(&e),
        ("jira.list_remote_links", json!({ "issue_key": "ACME-1" }))
    );
}

#[test]
fn a_ticket_s_development_reads_back_in_the_contract_s_shape() {
    let dev = Dev {
        prs: vec![DevPr {
            number: Some(12),
            url: "https://github.com/acme/app/pull/12".into(),
            title: "Retry".into(),
            state: "open".into(),
            checks: Some(Checks {
                passed: 14,
                total: 14,
                state: "pass".into(),
            }),
            author: None,
            repo: Some("acme/app".into()),
            updated: None,
        }],
        branches: vec![DevBranch {
            name: "acme-1".into(),
            url: None,
            repo: None,
        }],
        commits: Some(Commits {
            count: 2,
            latest: None,
        }),
        links: vec![],
        read_at: Utc.with_ymd_and_hms(2026, 11, 13, 9, 0, 0).unwrap(),
    };
    assert_eq!(
        serde_json::to_value(&dev).unwrap(),
        json!({
            "prs": [{ "number": 12, "url": "https://github.com/acme/app/pull/12", "title": "Retry",
                      "state": "open", "checks": { "passed": 14, "total": 14, "state": "pass" },
                      "repo": "acme/app" }],
            "branches": [{ "name": "acme-1" }],
            "commits": { "count": 2 },
            "links": [],
            "readAt": "2026-11-13T09:00:00Z"
        })
    );
}
