//! `GET /api/desk/folders` on the hub's own desk: an admin picks a folder
//! under the hub user's home, and nothing outside it, nobody else.

use serde_json::Value;
use workforce_identity::types::{Identity, IdentityRole};
use workforce_postgres::test_support::with_test_pool;

use crate::support::{agent_service, token_for, Gateway};

#[tokio::test(flavor = "multi_thread")]
async fn an_admin_lists_folders_under_home_and_no_path_leads_out() {
    let pg = std::sync::OnceLock::new();
    with_test_pool(
        |config| {
            let _ = pg.set(config.clone());
            workforce_desk::store::build_pool(config)
        },
        |_| run(pg.get().cloned().expect("the test database")),
    )
    .await;
}

async fn run(pg: workforce_postgres::PostgresConfig) {
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.postgres = Some(pg);
    config.desk.enabled = true;
    config.desk.adopt_interval_secs = 0;
    config.desk.restore = workforce_common::settings::DeskRestore::Off;
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(vec![("scout", agent_service(dir.path()).await)], None);
    let member = Identity::new("Member", IdentityRole::Member);
    gateway
        .state
        .identity_store
        .as_ref()
        .unwrap()
        .create(&member)
        .unwrap();
    let app = workforce_hub::desk_driver::mount(
        axum::Router::new(),
        &gateway.state,
        &config,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect("the hub's desk");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    // A folder of the test's own inside the hub user's home, and one outside.
    let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("a home folder"))
        .canonicalize()
        .unwrap();
    let mine = tempfile::Builder::new()
        .prefix("folders-test-")
        .tempdir_in(&home)
        .unwrap();
    std::fs::create_dir_all(mine.path().join("repo/.git")).unwrap();
    std::fs::create_dir_all(mine.path().join(".dot")).unwrap();
    std::fs::write(mine.path().join("file.txt"), "x").unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::create_dir(elsewhere.path().join("secret")).unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), mine.path().join("out")).unwrap();
    let mine_name = mine
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let get = |token: Option<String>, path: String, hidden: bool| async move {
        let mut request = reqwest::Client::new()
            .get(format!("http://{addr}/api/desk/folders"))
            .query(&[("path", path), ("hidden", hidden.to_string())]);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (
            status,
            response.json::<Value>().await.unwrap_or(Value::Null),
        )
    };
    let admin = Some(gateway.admin_token());

    let (status, body) = get(admin.clone(), mine_name.clone(), false).await;
    assert_eq!(status, 200, "{body}");
    let names: Vec<_> = body["folders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["repo"],
        "folders only, hidden ones left out, and the link out too"
    );
    assert_eq!(body["folders"][0]["git"], true);
    let (_, body) = get(admin.clone(), mine_name.clone(), true).await;
    assert_eq!(body["folders"][0]["name"], ".dot");

    let (status, body) = get(admin.clone(), String::new(), false).await;
    assert_eq!(status, 200, "defaults to home: {body}");
    assert_eq!(body["path"], home.to_string_lossy().as_ref());

    for path in [
        "..".to_string(),
        format!("{mine_name}/../.."),
        format!("{mine_name}/out"),
        format!("{mine_name}/out/secret"),
        elsewhere.path().to_string_lossy().into_owned(),
        "/etc".to_string(),
    ] {
        let (status, body) = get(admin.clone(), path.clone(), false).await;
        assert_eq!(status, 403, "{path}: {body}");
    }
    let (status, _) = get(admin.clone(), format!("{mine_name}/file.txt"), false).await;
    assert_eq!(status, 400);

    let (status, _) = get(Some(token_for(&member)), String::new(), false).await;
    assert_eq!(status, 403, "a member is not an admin");
    let (status, _) = get(None, String::new(), false).await;
    assert_eq!(status, 401);
}
