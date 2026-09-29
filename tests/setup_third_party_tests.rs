use std::{fs, path::Path};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{
        Method, Request, Response, StatusCode,
        header::{CONTENT_TYPE, COOKIE, SET_COOKIE},
    },
};
use dst_admin_rust::{
    domain::auth::SessionStore,
    infra::command::{CommandOutput, FakeCommandRunner},
    infra::config::{AppConfig, AutoUpdateModinfoConfig},
    infra::db::{connect_sqlite_memory, migrate},
    infra::http_client::{FakeHttpClient, HttpResponse},
    infra::process::SystemProcessSnapshotProvider,
    web::app::{AppState, build_router},
};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};
use tower::ServiceExt;

#[tokio::test]
async fn init_post_writes_user_info_first_marker_and_base_cluster_files() {
    let (app, dir, _http, _commands) = test_router(Vec::new(), Vec::new()).await;

    let response = send(
        &app,
        Method::POST,
        "/api/init",
        Some(json!({
            "userInfo": {
                "username": "owner",
                "password": "secret",
                "displayName": "PanelAdmin",
                "photoURL": "https://example.test/avatar.png"
            },
            "dstConfig": {
                "cluster": "IgnoredByGo"
            }
        })),
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_json(response).await,
        json!({"code": 200, "msg": "success", "data": null})
    );
    assert!(dir.path().join("first").is_file());
    assert_eq!(
        fs::read_to_string(dir.path().join("password.txt")).unwrap(),
        "username=owner\npassword=secret\ndisplayName=PanelAdmin\nphotoURL=https://example.test/avatar.png\n"
    );

    let cluster_dir = dir
        .path()
        .join(".klei")
        .join("DoNotStarveTogether")
        .join("MyDediServer");
    assert!(cluster_dir.join("cluster.ini").is_file());
    assert!(cluster_dir.join("cluster_token.txt").is_file());
    assert!(cluster_dir.join("Master/server.ini").is_file());
    assert!(cluster_dir.join("Caves/server.ini").is_file());
    assert!(
        fs::read_to_string(cluster_dir.join("cluster.ini"))
            .unwrap()
            .contains("cluster_name = PanelAdmin的世界")
    );
    assert_eq!(
        fs::read_to_string(cluster_dir.join("Master/server.ini")).unwrap(),
        include_str!("../static/Master/server.ini")
    );
    assert_eq!(
        fs::read_to_string(cluster_dir.join("Caves/server.ini")).unwrap(),
        include_str!("../static/Caves/server.ini")
    );
    assert_eq!(
        fs::read_to_string(cluster_dir.join("Master/leveldataoverride.lua")).unwrap(),
        include_str!("../static/Master/leveldataoverride.lua")
    );

    let second = send(
        &app,
        Method::POST,
        "/api/init",
        Some(json!({"userInfo": {"username": "owner"}})),
        None,
    )
    .await;
    assert_eq!(second.status(), StatusCode::OK);
    let second_body = response_json(second).await;
    assert_eq!(second_body["code"], "500");
    assert!(second_body["msg"].as_str().unwrap().contains("非法请求"));
}

#[tokio::test]
async fn install_steamcmd_streams_events_and_uses_fake_command_runner() {
    #[cfg(windows)]
    let http_responses = {
        let mut cursor = std::io::Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(&mut cursor);
        writer
            .start_file("steamcmd.exe", zip::write::SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut writer, b"fake executable").unwrap();
        writer.finish().unwrap();
        vec![HttpResponse::new(200).body(cursor.into_inner())]
    };
    #[cfg(not(windows))]
    let http_responses = Vec::new();
    let (app, dir, _http, commands) = test_router(
        http_responses,
        vec![CommandOutput::success(
            b"install ok\n".to_vec(),
            b"warning line\n".to_vec(),
        )],
    )
    .await;
    #[cfg(unix)]
    let script_path = dir.path().join("static/script/install_steamcmd.sh");
    #[cfg(unix)]
    {
        fs::create_dir_all(script_path.parent().unwrap()).unwrap();
        fs::write(&script_path, "#!/bin/sh\necho ok\n").unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    let response = send(&app, Method::GET, "/api/install/steamcmd", None, None).await;

    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(content_type.starts_with("text/event-stream"));
    let body = response_text(response).await;
    assert!(body.contains("data: 正在安装steamcmd"));
    assert!(body.contains("data: install ok"));
    assert!(body.contains("data: warning line"));
    assert!(body.contains("data: [successed]"));
    assert!(body.contains("data: end"));

    let calls = commands.calls();
    assert_eq!(calls.len(), 1);
    #[cfg(unix)]
    assert!(
        calls[0]
            .program()
            .ends_with("static/script/install_steamcmd.sh")
    );
    #[cfg(windows)]
    assert!(calls[0].program().ends_with("steamcmd\\steamcmd.exe"));
    #[cfg(windows)]
    assert!(dir.path().join("steamcmd/steamcmd.exe").is_file());
    #[cfg(unix)]
    assert_eq!(
        calls[0].args(),
        &[
            dir.path().display().to_string(),
            dir.path().display().to_string()
        ]
    );
    #[cfg(windows)]
    assert_eq!(calls[0].args()[0], "+force_install_dir");
    #[cfg(unix)]
    assert_ne!(
        fs::metadata(&script_path).unwrap().permissions().mode() & 0o100,
        0,
        "install handler should add the executable bit like Go's Chmod helper"
    );
    let dst_config = fs::read_to_string(dir.path().join("dst_config")).unwrap();
    assert!(dst_config.contains(&format!(
        "steamcmd={}",
        dir.path().join("steamcmd").display()
    )));
    assert!(dst_config.contains(&format!(
        "force_install_dir={}",
        dir.path().join("dst-dedicated-server").display()
    )));
    assert!(dst_config.contains("cluster=MyDediServer"));
}

#[tokio::test]
async fn install_steamcmd_requires_auth_after_first_run_marker_exists() {
    let (app, dir, _http, commands) = test_router(Vec::new(), Vec::new()).await;
    fs::write(dir.path().join("first"), "").unwrap();

    let response = send(&app, Method::GET, "/api/install/steamcmd", None, None).await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(commands.calls().is_empty());
}

#[tokio::test]
async fn third_party_proxy_routes_forward_raw_responses_and_transform_news() {
    let (app, _dir, http, _commands) = test_router(
        vec![
            HttpResponse::new(200)
                .header("content-type", "text/plain")
                .body("621987"),
            HttpResponse::new(200)
                .header("content-type", "application/json")
                .body(r#"{"servers":[{"name":"A"}]}"#),
            HttpResponse::new(200)
                .header("content-type", "application/json")
                .body(r#"{"detail":{"name":"A"}}"#),
            HttpResponse::new(200)
                .header("content-type", "application/json")
                .body(r#"{"GET":[{"__rowId":"row-1","name":"Lobby","players":"return { { name = \"Alice\", prefab = \"wilson\", netID = \"KU_A\", colour = \"red\", eventlevel = 2 } }","data":"return { day = 12, dayselapsedinseason = 3, daysleftinseason = 17 }","secondaries":{"caves":{"__addr":"127.0.0.1","id":"Caves","steamid":"steam","port":11001}}}]}"#),
            HttpResponse::new(200)
                .header("content-type", "application/json")
                .body(r#"{"data":[{"name":"B"}]}"#),
            HttpResponse::new(200)
                .header("content-type", "application/json")
                .body(r#"{"name":"Detail2"}"#),
            HttpResponse::new(200)
                .header("content-type", "application/json")
                .body(r#"{"appnews":{"newsitems":[{"feed_type":1,"title":"Patch","url":"https://example.test/news","date":1710000000},{"feed_type":0,"title":"Skip","url":"https://example.test/skip","date":1}]}}"#),
        ],
        Vec::new(),
    )
    .await;
    let cookie = login(&app).await;

    let version = send(&app, Method::GET, "/api/dst/version", None, Some(&cookie)).await;
    assert_eq!(version.status(), StatusCode::OK);
    assert_eq!(response_text(version).await, "621987");

    let home = send(
        &app,
        Method::POST,
        "/api/dst/home/server",
        Some(json!({
            "page": 2,
            "paginate": 20,
            "sort_type": "connected",
            "sort_way": 1,
            "search_type": 0,
            "search_content": "forest",
            "mode": "survival",
            "season": "autumn",
            "pvp": -1,
            "mod": 1,
            "password": 0,
            "world": -1,
            "playerpercent": "0-50"
        })),
        Some(&cookie),
    )
    .await;
    assert_eq!(home.status(), StatusCode::OK);
    assert_eq!(
        response_json(home).await,
        json!({"servers": [{"name": "A"}]})
    );

    let detail = send(
        &app,
        Method::POST,
        "/api/dst/home/server/detail",
        Some(json!({"rowId": "row-1", "region": "ap-southeast-1"})),
        Some(&cookie),
    )
    .await;
    assert_eq!(detail.status(), StatusCode::OK);
    assert_eq!(
        response_json(detail).await,
        json!({"detail": {"name": "A"}})
    );

    let lobby = send(
        &app,
        Method::GET,
        "/api/dst/lobby/server/detail?region=ap-southeast-1&rowId=row-1",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(lobby.status(), StatusCode::OK);
    let lobby_body = response_json(lobby).await;
    assert_eq!(lobby_body["code"], 200);
    assert_eq!(lobby_body["data"]["name"], "Lobby");
    assert_eq!(lobby_body["data"]["playerList"][0]["name"], "Alice");
    assert_eq!(lobby_body["data"]["dayData"]["day"], 12);
    assert!(
        lobby_body["data"]["secondariesJson"]
            .as_str()
            .unwrap()
            .contains("Caves")
    );

    let list2 = send(
        &app,
        Method::GET,
        "/api/dst/home/server2?current=3&pageSize=30&Name=Cave",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(list2.status(), StatusCode::OK);
    assert_eq!(response_json(list2).await, json!({"data": [{"name": "B"}]}));

    let detail2 = send(
        &app,
        Method::GET,
        "/api/dst/home/server/detail2?rowId=row-2",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(detail2.status(), StatusCode::OK);
    assert_eq!(response_json(detail2).await, json!({"name": "Detail2"}));

    let news = send(&app, Method::GET, "/steam/dst/news", None, None).await;
    assert_eq!(news.status(), StatusCode::OK);
    assert_eq!(
        response_json(news).await,
        json!({"code": 200, "msg": "success", "data": [{"title": "Patch", "url": "https://example.test/news", "date": 1710000000.0}]})
    );

    let calls = http.requests();
    assert_eq!(calls.len(), 7);
    assert_eq!(calls[0].method, "GET");
    assert_eq!(calls[0].url, "http://ver.tugos.cn/getLocalVersion");
    assert_eq!(calls[1].method, "POST");
    assert_eq!(
        calls[1].url,
        "http://dst.liuyh.com/index/serverlist/getserverlist.html"
    );
    let home_payload: Value = serde_json::from_slice(&calls[1].body).unwrap();
    assert_eq!(home_payload["page"], 2);
    assert_eq!(home_payload["paginate"], 20);
    assert_eq!(home_payload["search_content"], "forest");
    assert!(home_payload.get("pvp").is_none());
    assert_eq!(home_payload["mod"], 1);
    assert_eq!(home_payload["password"], 0);
    assert!(home_payload.get("world").is_none());
    assert_eq!(
        calls[2].url,
        "http://dst.liuyh.com/index/serverlist/getserverdetail.html"
    );
    assert_eq!(
        calls[3].url,
        "https://lobby-v2-ap-southeast-1.klei.com/lobby/read"
    );
    assert_eq!(
        calls[4].url,
        "https://api.dstserverlist.top/api/list?name=Cave&page=3&pageCount=30"
    );
    assert_eq!(
        calls[5].url,
        "https://api.dstserverlist.top/api/details/row-2"
    );
    assert_eq!(
        calls[6].url,
        "https://steamcommunity-a.akamaihd.net/news/newsforapp/v0002/?appid=322330&count=10&maxlength=300&format=json"
    );
}

#[tokio::test]
async fn third_party_proxy_routes_return_503_for_upstream_failures() {
    let (app, _dir, _http, _commands) =
        test_router(vec![HttpResponse::new(502).body("bad gateway")], Vec::new()).await;
    let cookie = login(&app).await;

    let response = send(&app, Method::GET, "/api/dst/version", None, Some(&cookie)).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn third_party_proxy_routes_cover_transport_malformed_and_oversize_failures() {
    let (app, _dir, _http, _commands) = test_router(Vec::new(), Vec::new()).await;
    let cookie = login(&app).await;

    let exhausted = send(&app, Method::GET, "/api/dst/version", None, Some(&cookie)).await;
    assert_eq!(exhausted.status(), StatusCode::SERVICE_UNAVAILABLE);

    let oversized_body = "x".repeat(10 * 1024 * 1024 + 1);
    let (app, _dir, _http, _commands) = test_router(
        vec![HttpResponse::new(200).body(oversized_body)],
        Vec::new(),
    )
    .await;
    let cookie = login(&app).await;
    let oversized = send(&app, Method::GET, "/api/dst/version", None, Some(&cookie)).await;
    assert_eq!(oversized.status(), StatusCode::SERVICE_UNAVAILABLE);

    let (app, _dir, _http, _commands) =
        test_router(vec![HttpResponse::new(200).body("not-json")], Vec::new()).await;
    let cookie = login(&app).await;
    let lobby = send(
        &app,
        Method::GET,
        "/api/dst/lobby/server/detail?region=ap-southeast-1&rowId=row-1",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(lobby.status(), StatusCode::OK);
    let body = response_json(lobby).await;
    assert_eq!(body["code"], 200);
    assert_eq!(body["msg"], "success");
    assert_eq!(body["data"]["name"], "");
    assert_eq!(body["data"]["playerList"], Value::Null);
    assert_eq!(body["data"]["dayData"]["day"], 0);
    assert_eq!(body["data"]["secondariesJson"], "");
}

async fn test_router(
    http_responses: Vec<HttpResponse>,
    command_outputs: Vec<CommandOutput>,
) -> (Router, TempDir, FakeHttpClient, FakeCommandRunner) {
    let dir = tempdir().unwrap();
    write_password_file(dir.path());
    let pool = connect_sqlite_memory().await.unwrap();
    migrate(&pool).await.unwrap();
    let http = FakeHttpClient::new(http_responses);
    let commands = FakeCommandRunner::new(command_outputs);
    let state = AppState::new_with_command_runner_and_http_client(
        test_config(),
        pool,
        SessionStore::new(),
        dir.path(),
        commands.clone(),
        http.clone(),
        SystemProcessSnapshotProvider,
    );
    (build_router(state), dir, http, commands)
}

fn test_config() -> AppConfig {
    AppConfig {
        bind_address: "127.0.0.1".to_owned(),
        port: "0".to_owned(),
        path: String::new(),
        data_dir: String::new(),
        database: ":memory:".to_owned(),
        steamcmd: String::new(),
        steam_api_key: None,
        flag: String::new(),
        wan_ip: String::new(),
        white_admin_ip: None,
        token: Some("cluster-token".to_owned()),
        dst_version_url: "https://example.test/version".to_owned(),
        auto_update_modinfo: AutoUpdateModinfoConfig {
            enable: false,
            check_interval: 5,
            update_check_interval: 10,
        },
        dst_cli_port: String::new(),
    }
}

async fn login(app: &Router) -> String {
    let response = send(
        app,
        Method::POST,
        "/api/login",
        Some(json!({"username": "admin", "password": "123456"})),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    response
        .headers()
        .get(SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

fn write_password_file(root_path: &Path) {
    fs::write(
        root_path.join("password.txt"),
        "username=admin\npassword=123456\ndisplayName=Admin\nphotoURL=https://example.test/avatar.png\n",
    )
    .unwrap();
}

async fn send(
    app: &Router,
    method: Method,
    uri: &str,
    json_body: Option<Value>,
    cookie: Option<&str>,
) -> Response<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if json_body.is_some() {
        builder = builder.header(CONTENT_TYPE, "application/json");
    }
    if let Some(cookie) = cookie {
        builder = builder.header(COOKIE, cookie);
    }
    let body = json_body
        .map(|value| Body::from(value.to_string()))
        .unwrap_or_else(Body::empty);
    app.clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap()
}

async fn response_json(response: Response<Body>) -> Value {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("expected JSON response for status {status}: {error}"))
}

async fn response_text(response: Response<Body>) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}
