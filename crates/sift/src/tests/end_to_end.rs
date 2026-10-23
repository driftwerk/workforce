use super::{exec, Mock};
use crate::Outcome;
use serde_json::json;
use std::sync::Arc;

const PROGRAM: &str = r#"// Sign-offs per team at one organisation since a date.
let org = one(orgs_find({ name: "North Valley" }))
let since = "2026-06-01"
let signoffs = sql(org.db) {
  SELECT signer_id, signed_at::date AS day, status
  FROM sign_offs
  WHERE org_id = ${org.id} AND signed_at >= ${since}
}
let staff = sql(org.db) {
  SELECT id, name, team FROM staff WHERE org_id = ${org.id}
}
let by_team = group_by(join(signoffs, staff, s => s.signer_id, p => p.id), pair => pair.right.team)
let teams = sort_by_desc(by_team.map(g => {
  team: g.key,
  signed: count(g.items.filter(p => p.left.status == "signed")),
  people: count(group_by(g.items, p => p.right.id)),
}), t => t.signed)
return {
  org: org.name,
  since,
  total: count(signoffs),
  teams: table(teams),
  top: first(teams)?.team ?? "none",
}
"#;

#[tokio::test]
async fn find_an_org_query_twice_group_and_aggregate() {
    let mock = Arc::new(
        Mock::new()
            .tool("orgs_find", |args| {
                assert_eq!(args, &json!({ "name": "North Valley" }));
                Ok(json!([{ "id": 42, "name": "North Valley", "db": 7 }]))
            })
            .tool("orgs_list", |_| Ok(json!([])))
            .sql(|db, query, _| {
                assert_eq!(db, &json!(7));
                Ok(if query.contains("sign_offs") {
                    json!([
                        { "signer_id": 1, "day": "2026-06-02", "status": "signed" },
                        { "signer_id": 2, "day": "2026-06-03", "status": "signed" },
                        { "signer_id": 1, "day": "2026-06-04", "status": "pending" },
                        { "signer_id": 3, "day": "2026-06-05", "status": "signed" },
                        { "signer_id": 3, "day": "2026-06-06", "status": "signed" },
                        { "signer_id": 9, "day": "2026-06-07", "status": "signed" }
                    ])
                } else {
                    json!([
                        { "id": 1, "name": "Ann", "team": "unit a" },
                        { "id": 2, "name": "Bo", "team": "unit b" },
                        { "id": 3, "name": "Cy", "team": "unit b" }
                    ])
                })
            }),
    );
    let outcome = exec(&mock, PROGRAM).await;
    let Outcome::Returned { value, stats } = outcome else {
        panic!("{}", outcome.render(PROGRAM))
    };
    assert_eq!(
        value,
        json!({
            "org": "North Valley",
            "since": "2026-06-01",
            "total": 6,
            "teams": [
                { "team": "unit b", "signed": 3, "people": 2 },
                { "team": "unit a", "signed": 1, "people": 1 }
            ],
            "top": "unit b"
        })
    );
    let sql = mock.sql_calls();
    assert_eq!(sql.len(), 2);
    assert_eq!(
        sql[0].params,
        vec![
            ("p1".to_string(), json!(42)),
            ("p2".to_string(), json!("2026-06-01"))
        ]
    );
    assert!(sql[0]
        .query
        .contains("WHERE org_id = {{p1}} AND signed_at >= {{p2}}"));
    assert_eq!(sql[1].params, vec![("p1".to_string(), json!(42))]);
    assert_eq!(stats.calls, 3);
    let tools: Vec<_> = stats
        .journal
        .iter()
        .map(|e| (e.tool.as_str(), e.ok))
        .collect();
    assert_eq!(tools, [("orgs_find", true), ("sql", true), ("sql", true)]);
}
