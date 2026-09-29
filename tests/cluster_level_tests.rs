use std::{
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

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
    dst,
    infra::command::{CommandOutput, CommandRunner, FakeCommandRunner},
    infra::config::{AppConfig, AutoUpdateModinfoConfig},
    infra::db::{connect_sqlite_memory, migrate},
    infra::http_client::{FakeHttpClient, HttpResponse},
    infra::process::{ProcessSnapshot, ProcessSnapshotProvider},
    web::app::{AppState, build_router},
};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};
use tower::ServiceExt;

#[tokio::test]
async fn cluster_routes_create_page_update_and_delete_with_existing_dst_install() {
    let (app, dir) = test_router().await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterApi");
    fs::create_dir_all(dir.path().join("server")).unwrap();

    let created = send(
        &app,
        Method::POST,
        "/api/cluster",
        Some(json!({
            "clusterName": "ClusterApi",
            "description": "first shard",
            "steamcmd": "/opt/steamcmd",
            "force_install_dir": dir.path().join("server").display().to_string(),
            "backup": dir.path().join("backup").display().to_string(),
            "mod_download_path": dir.path().join("mods").display().to_string(),
            "uuid": "",
            "beta": 0,
            "bin": 64,
            "ugc_directory": "",
            "persistent_storage_root": "",
            "conf_dir": ""
        })),
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    assert_eq!(
        response_json(created).await,
        json!({"code": 200, "msg": "success", "data": null})
    );
    assert!(
        dir.path()
            .join(".klei/DoNotStarveTogether/ClusterApi/cluster.ini")
            .exists()
    );
    assert!(
        dir.path()
            .join(".klei/DoNotStarveTogether/ClusterApi/Master/server.ini")
            .exists()
    );

    let listed = send(
        &app,
        Method::GET,
        "/api/cluster?page=1&size=10",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(listed.status(), StatusCode::OK);
    let body = response_json(listed).await;
    assert_eq!(body["code"], 200);
    assert_eq!(body["msg"], "success");
    assert_eq!(body["data"]["page"], 1);
    assert_eq!(body["data"]["size"], 10);
    assert_eq!(body["data"]["total"], 1);
    assert_eq!(body["data"]["totalPages"], 1);
    assert_eq!(body["data"]["data"][0]["clusterName"], "ClusterApi");
    assert_eq!(body["data"]["data"][0]["description"], "first shard");
    assert_eq!(body["data"]["data"][0]["master"], false);
    assert_eq!(body["data"]["data"][0]["caves"], false);
    assert_eq!(body["data"]["data"][0]["password"], "");
    assert!(body["data"]["data"][0].get("Master").is_none());
    assert!(body["data"]["data"][0].get("Caves").is_none());
    assert!(body["data"]["data"][0].get("bin").is_none());
    let id = body["data"]["data"][0]["ID"].as_i64().unwrap();

    let updated = send(
        &app,
        Method::PUT,
        "/api/cluster",
        Some(json!({"ID": id, "description": "renamed"})),
        Some(&cookie),
    )
    .await;
    assert_eq!(updated.status(), StatusCode::OK);
    assert_eq!(
        response_json(updated).await,
        json!({"code": 200, "msg": "success", "data": null})
    );

    let deleted = send(
        &app,
        Method::DELETE,
        &format!("/api/cluster?id={id}"),
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::OK);
    assert_eq!(
        response_json(deleted).await,
        json!({"code": 200, "msg": "success", "data": null})
    );

    let listed = send(&app, Method::GET, "/api/cluster", None, Some(&cookie)).await;
    assert_eq!(response_json(listed).await["data"]["data"], json!([]));
}

#[tokio::test]
async fn failed_cluster_file_initialization_does_not_reserve_cluster_name() {
    let (app, dir) = test_router().await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterRetry");
    fs::create_dir_all(dir.path().join("server")).unwrap();
    let blocking_file = dir.path().join("not-a-directory");
    fs::write(&blocking_file, "file").unwrap();

    let failed = send(
        &app,
        Method::POST,
        "/api/cluster",
        Some(json!({
            "clusterName": "ClusterRetry",
            "description": "first attempt",
            "steamcmd": "/opt/steamcmd",
            "force_install_dir": dir.path().join("server").display().to_string(),
            "backup": blocking_file.join("backup").display().to_string(),
            "mod_download_path": dir.path().join("mods").display().to_string(),
            "uuid": "",
            "beta": 0,
            "bin": 64,
            "ugc_directory": "",
            "persistent_storage_root": "",
            "conf_dir": ""
        })),
        Some(&cookie),
    )
    .await;
    assert_eq!(failed.status(), StatusCode::BAD_REQUEST);
    assert!(
        !dir.path()
            .join(".klei/DoNotStarveTogether/ClusterRetry")
            .exists(),
        "configured path validation failures must not leave a cluster skeleton behind"
    );

    let retried = send(
        &app,
        Method::POST,
        "/api/cluster",
        Some(json!({
            "clusterName": "ClusterRetry",
            "description": "retry",
            "steamcmd": "/opt/steamcmd",
            "force_install_dir": dir.path().join("server").display().to_string(),
            "backup": dir.path().join("backup").display().to_string(),
            "mod_download_path": dir.path().join("mods").display().to_string(),
            "uuid": "",
            "beta": 0,
            "bin": 64,
            "ugc_directory": "",
            "persistent_storage_root": "",
            "conf_dir": ""
        })),
        Some(&cookie),
    )
    .await;
    assert_eq!(retried.status(), StatusCode::OK);

    let listed = send(&app, Method::GET, "/api/cluster", None, Some(&cookie)).await;
    assert_eq!(response_json(listed).await["data"]["total"], 1);

    write_dst_config(dir.path(), "ClusterFile");
    fs::write(
        dir.path().join(".klei/DoNotStarveTogether/ClusterFile"),
        "not a directory",
    )
    .unwrap();
    let failed_file_cluster = send(
        &app,
        Method::POST,
        "/api/cluster",
        Some(json!({
            "clusterName": "ClusterFile",
            "description": "file collision",
            "steamcmd": "/opt/steamcmd",
            "force_install_dir": dir.path().join("server").display().to_string(),
            "backup": dir.path().join("backup").display().to_string(),
            "mod_download_path": dir.path().join("mods").display().to_string(),
            "uuid": "",
            "beta": 0,
            "bin": 64,
            "ugc_directory": "",
            "persistent_storage_root": "",
            "conf_dir": ""
        })),
        Some(&cookie),
    )
    .await;
    assert_eq!(failed_file_cluster.status(), StatusCode::BAD_REQUEST);

    let listed = send(&app, Method::GET, "/api/cluster", None, Some(&cookie)).await;
    assert_eq!(response_json(listed).await["data"]["total"], 1);
}

#[cfg(unix)]
#[tokio::test]
async fn cluster_create_rejects_symlinked_configured_backup_and_mod_paths() {
    use std::os::unix::fs::symlink;

    let (app, dir) = test_router().await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterConfiguredPathLink");
    fs::create_dir_all(dir.path().join("server")).unwrap();
    let outside = dir.path().join("outside-configured-paths");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, dir.path().join("backup-link")).unwrap();
    symlink(&outside, dir.path().join("mods-link")).unwrap();

    let rejected = send(
        &app,
        Method::POST,
        "/api/cluster",
        Some(json!({
            "clusterName": "ClusterConfiguredPathLink",
            "description": "reject configured path links",
            "steamcmd": "/opt/steamcmd",
            "force_install_dir": dir.path().join("server").display().to_string(),
            "backup": dir.path().join("backup-link/nested").display().to_string(),
            "mod_download_path": dir.path().join("mods-link/nested").display().to_string(),
            "uuid": "",
            "beta": 0,
            "bin": 64,
            "ugc_directory": "",
            "persistent_storage_root": "",
            "conf_dir": ""
        })),
        Some(&cookie),
    )
    .await;

    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert!(!outside.join("nested").exists());
}

#[tokio::test]
async fn partial_existing_cluster_directory_is_repaired_on_create_retry() {
    let (app, dir) = test_router().await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterPartial");
    fs::create_dir_all(dir.path().join("server")).unwrap();
    let cluster_dir = dir.path().join(".klei/DoNotStarveTogether/ClusterPartial");
    fs::create_dir_all(cluster_dir.join("Master")).unwrap();

    let created = send(
        &app,
        Method::POST,
        "/api/cluster",
        Some(json!({
            "clusterName": "ClusterPartial",
            "description": "repairs partial files",
            "steamcmd": "/opt/steamcmd",
            "force_install_dir": dir.path().join("server").display().to_string(),
            "backup": dir.path().join("backup").display().to_string(),
            "mod_download_path": dir.path().join("mods").display().to_string(),
            "uuid": "",
            "beta": 0,
            "bin": 64,
            "ugc_directory": "",
            "persistent_storage_root": "",
            "conf_dir": ""
        })),
        Some(&cookie),
    )
    .await;

    assert_eq!(created.status(), StatusCode::OK);
    assert!(cluster_dir.join("cluster.ini").is_file());
    assert!(cluster_dir.join("cluster_token.txt").is_file());
    assert!(cluster_dir.join("Master/server.ini").is_file());
    assert!(cluster_dir.join("Caves/server.ini").is_file());
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_klei_root_is_rejected_without_writing_to_escape_target() {
    use std::os::unix::fs::symlink;

    let (app, dir) = test_router().await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterRootLink");
    let escaped_root = dir.path().join("escaped-klei-root");
    fs::create_dir(&escaped_root).unwrap();
    symlink(&escaped_root, dir.path().join(".klei")).unwrap();

    let rejected = send(
        &app,
        Method::POST,
        "/api/game/player/adminlist",
        Some(json!({"adminList": ["KU_root"]})),
        Some(&cookie),
    )
    .await;

    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert!(!escaped_root.join("DoNotStarveTogether").exists());
}

#[tokio::test]
async fn cluster_create_installs_dst_when_force_install_dir_is_missing() {
    let runner = FakeCommandRunner::new(vec![CommandOutput::success(Vec::new(), Vec::new())]);
    let calls = runner.clone();
    let (app, dir) = test_router_with_command_runner(runner).await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterInstall");
    let steamcmd = dir.path().join("steamcmd");
    fs::create_dir_all(&steamcmd).unwrap();
    let force_install_dir = dir.path().join("dst-server");

    let created = send(
        &app,
        Method::POST,
        "/api/cluster",
        Some(json!({
            "clusterName": "ClusterInstall",
            "description": "install missing server",
            "steamcmd": steamcmd.display().to_string(),
            "force_install_dir": force_install_dir.display().to_string(),
            "backup": dir.path().join("backup").display().to_string(),
            "mod_download_path": dir.path().join("mods").display().to_string(),
            "uuid": "",
            "beta": 0,
            "bin": 64,
            "ugc_directory": "",
            "persistent_storage_root": "",
            "conf_dir": ""
        })),
        Some(&cookie),
    )
    .await;

    assert_eq!(created.status(), StatusCode::OK);
    let calls = calls.calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(
        call.program(),
        if cfg!(windows) {
            "steamcmd.exe"
        } else {
            "./steamcmd.sh"
        }
    );
    assert_ne!(call.program(), "sh");
    assert_ne!(call.program(), "bash");
    assert_eq!(call.current_dir(), Some(steamcmd.as_path()));
    assert_eq!(
        call.args(),
        [
            "+login",
            "anonymous",
            "+force_install_dir",
            force_install_dir.to_str().unwrap(),
            "+app_update",
            "343050",
            "validate",
            "+quit"
        ]
    );
}

#[tokio::test]
async fn cluster_create_rolls_back_database_row_when_dst_install_fails() {
    let runner = FakeCommandRunner::new(vec![CommandOutput {
        status_code: Some(1),
        stdout: Vec::new(),
        stderr: b"failed".to_vec(),
        timed_out: false,
        stdout_truncated: false,
        stderr_truncated: false,
    }]);
    let (app, dir) = test_router_with_command_runner(runner).await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterInstallFail");
    let steamcmd = dir.path().join("steamcmd");
    fs::create_dir_all(&steamcmd).unwrap();

    let failed = send(
        &app,
        Method::POST,
        "/api/cluster",
        Some(json!({
            "clusterName": "ClusterInstallFail",
            "description": "install fails",
            "steamcmd": steamcmd.display().to_string(),
            "force_install_dir": dir.path().join("dst-server").display().to_string(),
            "backup": dir.path().join("backup").display().to_string(),
            "mod_download_path": dir.path().join("mods").display().to_string(),
            "uuid": "",
            "beta": 0,
            "bin": 64,
            "ugc_directory": "",
            "persistent_storage_root": "",
            "conf_dir": ""
        })),
        Some(&cookie),
    )
    .await;

    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let listed = send(&app, Method::GET, "/api/cluster", None, Some(&cookie)).await;
    assert_eq!(response_json(listed).await["data"]["data"], json!([]));
}

#[tokio::test]
async fn cluster_list_reports_master_and_caves_status_from_process_provider() {
    let processes = FakeProcessSnapshotProvider::new(vec![
        process_snapshot("ClusterRuntime", "Master"),
        process_snapshot("ClusterRuntime", "Caves"),
    ]);
    let (app, dir, _http) = test_router_with_processes_and_http(processes, Vec::new()).await;
    let cookie = login(&app).await;
    seed_cluster_row(&app, dir.path(), &cookie, "ClusterRuntime").await;

    let listed = send(&app, Method::GET, "/api/cluster", None, Some(&cookie)).await;

    assert_eq!(listed.status(), StatusCode::OK);
    let body = response_json(listed).await;
    let cluster = &body["data"]["data"][0];
    assert_eq!(cluster["clusterName"], "ClusterRuntime");
    assert_eq!(cluster["master"], true);
    assert_eq!(cluster["caves"], true);
}

#[tokio::test]
async fn cluster_list_collects_process_snapshots_once_for_multiple_rows() {
    let processes = FakeProcessSnapshotProvider::new(vec![
        process_snapshot("ClusterRuntimeOne", "Master"),
        process_snapshot("ClusterRuntimeTwo", "Caves"),
    ]);
    let calls = processes.clone();
    let (app, dir, _http) = test_router_with_processes_and_http(processes, Vec::new()).await;
    let cookie = login(&app).await;
    seed_cluster_row(&app, dir.path(), &cookie, "ClusterRuntimeOne").await;
    seed_cluster_row(&app, dir.path(), &cookie, "ClusterRuntimeTwo").await;

    let listed = send(&app, Method::GET, "/api/cluster", None, Some(&cookie)).await;

    assert_eq!(listed.status(), StatusCode::OK);
    assert_eq!(response_json(listed).await["data"]["total"], 2);
    assert_eq!(calls.calls(), 1);
}

#[tokio::test]
async fn cluster_list_reuses_lobby_lookup_without_dropping_large_page_fields() {
    const LARGE_PAGE_SIZE: usize = 105;
    let upstream = json!({
        "success": true,
        "successinfo": {
            "data": [
                [
                    "row-shared",
                    null,
                    null,
                    null,
                    null,
                    2,
                    8,
                    null,
                    "survival",
                    1,
                    "Shared Lobby",
                    false,
                    null,
                    null,
                    "spring",
                    null,
                    null,
                    null,
                    null,
                    null,
                    "US"
                ]
            ]
        }
    });
    let escaped_body = serde_json::to_string(&upstream.to_string()).unwrap();
    let (app, dir, http) = test_router_with_processes_and_http(
        FakeProcessSnapshotProvider::default(),
        vec![HttpResponse::new(200).body(escaped_body)],
    )
    .await;
    let cookie = login(&app).await;
    for index in 0..LARGE_PAGE_SIZE {
        let cluster_name = format!("ClusterBounded{index}");
        seed_cluster_row(&app, dir.path(), &cookie, &cluster_name).await;
        fs::write(
            dir.path()
                .join(".klei/DoNotStarveTogether")
                .join(cluster_name)
                .join("cluster.ini"),
            "[GAMEPLAY]\n\
game_mode = survival\n\
max_players = 8\n\
\n\
[NETWORK]\n\
cluster_name = Shared Lobby\n",
        )
        .unwrap();
    }

    let listed = send(
        &app,
        Method::GET,
        "/api/cluster?size=105",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(listed.status(), StatusCode::OK);
    let rows = response_json(listed).await["data"]["data"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(rows.len(), LARGE_PAGE_SIZE);
    assert!(rows.iter().all(|row| row["rowId"] == "row-shared"));
    assert!(rows.iter().all(|row| row["connected"] == 2));
    assert_eq!(http.requests().len(), 1);
}

#[tokio::test]
async fn cluster_list_enriches_lobby_fields_from_go_server_list_response() {
    let upstream = json!({
        "success": true,
        "successinfo": {
            "data": [
                [
                    "row-ignored",
                    null,
                    null,
                    null,
                    null,
                    1,
                    6,
                    null,
                    "endless",
                    3,
                    "Different Lobby",
                    false,
                    null,
                    null,
                    "autumn",
                    null,
                    null,
                    null,
                    null,
                    null,
                    "EU"
                ],
                [
                    "row-42",
                    null,
                    null,
                    null,
                    null,
                    4,
                    10,
                    null,
                    "survival",
                    7,
                    "Runtime Lobby",
                    true,
                    null,
                    null,
                    "winter",
                    null,
                    null,
                    null,
                    null,
                    null,
                    "CN"
                ]
            ]
        }
    });
    let escaped_body = serde_json::to_string(&upstream.to_string()).unwrap();
    let (app, dir, http) = test_router_with_processes_and_http(
        FakeProcessSnapshotProvider::default(),
        vec![HttpResponse::new(200).body(escaped_body)],
    )
    .await;
    let cookie = login(&app).await;
    seed_cluster_row(&app, dir.path(), &cookie, "ClusterLobby").await;
    let cluster_dir = dir.path().join(".klei/DoNotStarveTogether/ClusterLobby");
    fs::write(
        cluster_dir.join("cluster.ini"),
        "[GAMEPLAY]\n\
game_mode = survival\n\
max_players = 10\n\
\n\
[NETWORK]\n\
cluster_name = Runtime Lobby\n\
cluster_password = secret\n",
    )
    .unwrap();

    let listed = send(&app, Method::GET, "/api/cluster", None, Some(&cookie)).await;

    assert_eq!(listed.status(), StatusCode::OK);
    let body = response_json(listed).await;
    let cluster = &body["data"]["data"][0];
    assert_eq!(cluster["rowId"], "row-42");
    assert_eq!(cluster["connected"], 4);
    assert_eq!(cluster["maxConnections"], 10);
    assert_eq!(cluster["mode"], "survival");
    assert_eq!(cluster["mods"], 7);
    assert_eq!(cluster["season"], "winter");
    assert_eq!(cluster["region"], "CN");
    assert_eq!(cluster["password"], "");

    let requests = http.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(
        requests[0].url,
        "https://dst.liuyh.com/index/serverlist/getserverlist.html"
    );
    assert!(
        requests[0]
            .headers
            .contains(&("X-Requested-With".to_owned(), "XMLHttpRequest".to_owned()))
    );
    assert!(
        requests[0]
            .headers
            .contains(&("Content-Type".to_owned(), "application/json".to_owned()))
    );
    let request_body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        request_body,
        json!({
            "page": 1,
            "paginate": 10,
            "sort_type": "name",
            "sort_way": 1,
            "search_type": 1,
            "search_content": "Runtime Lobby",
            "mod": 1
        })
    );
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_existing_klei_root_is_rejected_before_reading_or_writing() {
    use std::os::unix::fs::symlink;

    let (app, dir) = test_router().await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterRootExistingLink");
    let escaped_root = dir.path().join("escaped-existing-klei-root");
    fs::create_dir_all(escaped_root.join("DoNotStarveTogether/ClusterRootExistingLink")).unwrap();
    symlink(&escaped_root, dir.path().join(".klei")).unwrap();

    let rejected = send(
        &app,
        Method::GET,
        "/api/game/player/adminlist",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
}

#[cfg(unix)]
#[tokio::test]
async fn dst_config_default_paths_do_not_create_directories_through_symlinked_klei_root() {
    use std::os::unix::fs::symlink;

    let (app, dir) = test_router().await;
    let cookie = login(&app).await;
    fs::write(
        dir.path().join("dst_config"),
        "cluster=ClusterDefaultPaths\n",
    )
    .unwrap();
    let escaped_root = dir.path().join("escaped-default-paths");
    fs::create_dir(&escaped_root).unwrap();
    symlink(&escaped_root, dir.path().join(".klei")).unwrap();

    let rejected = send(
        &app,
        Method::GET,
        "/api/game/player/adminlist",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert!(!escaped_root.join("DoNotStarveTogether/backup").exists());
    assert!(
        !escaped_root
            .join("DoNotStarveTogether/mod_config_download")
            .exists()
    );
}

#[test]
fn init_cluster_files_repairs_partial_existing_directory() {
    let dir = tempdir().unwrap();
    write_dst_config(dir.path(), "ClusterDirectPartial");
    let cluster_dir = dir
        .path()
        .join(".klei/DoNotStarveTogether/ClusterDirectPartial");
    fs::create_dir_all(cluster_dir.join("Master")).unwrap();

    dst::init_cluster_files(dir.path(), "ClusterDirectPartial", "token").unwrap();

    assert!(cluster_dir.join("cluster.ini").is_file());
    assert_eq!(
        fs::read_to_string(cluster_dir.join("cluster_token.txt")).unwrap(),
        "token"
    );
    assert!(cluster_dir.join("Master/modoverrides.lua").is_file());
    assert!(cluster_dir.join("Caves/modoverrides.lua").is_file());
}

#[tokio::test]
async fn cluster_ini_and_level_routes_round_trip_dst_files() {
    let (app, dir) = test_router().await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterFiles");

    let saved_cluster = send(
        &app,
        Method::POST,
        "/api/game/8level/clusterIni",
        Some(json!({
            "cluster": {
                "game_mode": "survival",
                "max_players": 12,
                "pvp": true,
                "pause_when_nobody": false,
                "vote_enabled": true,
                "vote_kick_enabled": false,
                "lan_only_cluster": false,
                "cluster_intention": "cooperative",
                "cluster_description": "file backed config",
                "cluster_password": "secret",
                "cluster_name": "Wendy World",
                "offline_cluster": false,
                "cluster_language": "zh",
                "whitelist_slots": 1,
                "tick_rate": 30,
                "console_enabled": true,
                "max_snapshots": 8,
                "shard_enabled": true,
                "bind_ip": "0.0.0.0",
                "master_ip": "127.0.0.1",
                "master_port": 10888,
                "cluster_key": "shared-key",
                "steam_group_id": "",
                "steam_group_only": false,
                "steam_group_admins": false
            },
            "token": "cluster-token"
        })),
        Some(&cookie),
    )
    .await;
    assert_eq!(saved_cluster.status(), StatusCode::OK);
    assert_eq!(
        response_json(saved_cluster).await["data"]["token"],
        "cluster-token"
    );

    let cluster_dir = dir.path().join(".klei/DoNotStarveTogether/ClusterFiles");
    assert!(
        fs::read_to_string(cluster_dir.join("cluster.ini"))
            .unwrap()
            .contains("cluster_name = Wendy World")
    );
    assert_eq!(
        fs::read_to_string(cluster_dir.join("cluster_token.txt")).unwrap(),
        "cluster-token"
    );

    let saved_level = send(
        &app,
        Method::POST,
        "/api/cluster/level",
        Some(json!({
            "levelName": "Shard One",
            "is_master": false,
            "uuid": "ShardOne",
            "leveldataoverride": "return { override_enabled = true }",
            "modoverrides": "return { [\"workshop-123\"] = { enabled = true } }",
            "server_ini": {
                "server_port": 11001,
                "is_master": false,
                "name": "ShardOne",
                "id": 10011,
                "encode_user_path": true,
                "authentication_port": 8767,
                "master_server_port": 27017
            }
        })),
        Some(&cookie),
    )
    .await;
    assert_eq!(saved_level.status(), StatusCode::OK);
    assert_eq!(response_json(saved_level).await["data"]["uuid"], "ShardOne");

    let level_dir = cluster_dir.join("ShardOne");
    assert_eq!(
        fs::read_to_string(level_dir.join("leveldataoverride.lua")).unwrap(),
        "return { override_enabled = true }"
    );
    assert!(
        fs::read_to_string(level_dir.join("server.ini"))
            .unwrap()
            .contains("server_port = 11001")
    );
    let updated_levels = send(
        &app,
        Method::PUT,
        "/api/cluster/level",
        Some(json!({
            "levels": [{
                "levelName": "Master",
                "is_master": true,
                "uuid": "Master",
                "leveldataoverride": "return { preset = \"SURVIVAL_TOGETHER\" }",
                "modoverrides": "return { [\"workshop-789\"] = { enabled = true } }",
                "server_ini": {
                    "server_port": 11000,
                    "is_master": true,
                    "name": "Master",
                    "id": 1,
                    "encode_user_path": true,
                    "authentication_port": 8766,
                    "master_server_port": 27016
                }
            }]
        })),
        Some(&cookie),
    )
    .await;
    assert_eq!(updated_levels.status(), StatusCode::OK);
    assert_eq!(
        fs::read_to_string(
            dir.path()
                .join("server/mods/dedicated_server_mods_setup.lua")
        )
        .unwrap(),
        "ServerModSetup(\"789\")\n"
    );
    assert_eq!(
        fs::read_to_string(cluster_dir.join("level.json")).unwrap(),
        r#"{"levelList":[{"name":"Master","file":"Master"}]}"#
    );

    let levels = send(&app, Method::GET, "/api/cluster/level", None, Some(&cookie)).await;
    assert_eq!(levels.status(), StatusCode::OK);
    let levels = response_json(levels).await;
    assert_eq!(levels["data"][0]["levelName"], "Master");
    assert_eq!(levels["data"][0]["server_ini"]["server_port"], 11000);
}

#[tokio::test]
async fn malformed_level_index_returns_error_without_reinitializing_file() {
    let (app, dir) = test_router().await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterMalformed");
    let cluster_dir = dir
        .path()
        .join(".klei/DoNotStarveTogether/ClusterMalformed");
    fs::create_dir_all(&cluster_dir).unwrap();
    fs::write(cluster_dir.join("level.json"), "{broken").unwrap();

    let response = send(&app, Method::GET, "/api/cluster/level", None, Some(&cookie)).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        fs::read_to_string(cluster_dir.join("level.json")).unwrap(),
        "{broken"
    );
}

#[tokio::test]
async fn level_delete_validates_index_before_removing_directory() {
    let (app, dir) = test_router().await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterDeleteMalformedIndex");
    let cluster_dir = dir
        .path()
        .join(".klei/DoNotStarveTogether/ClusterDeleteMalformedIndex");
    fs::create_dir_all(cluster_dir.join("Master")).unwrap();
    fs::write(cluster_dir.join("Master/keep.txt"), "keep").unwrap();
    fs::write(cluster_dir.join("level.json"), "{broken").unwrap();

    let rejected = send(
        &app,
        Method::DELETE,
        "/api/cluster/level?levelName=Master",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(rejected.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        fs::read_to_string(cluster_dir.join("Master/keep.txt")).unwrap(),
        "keep"
    );
    assert_eq!(
        fs::read_to_string(cluster_dir.join("level.json")).unwrap(),
        "{broken"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn level_delete_restores_staged_directory_when_index_save_fails() {
    use std::os::unix::fs::PermissionsExt;

    let (app, dir) = test_router().await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterDeleteReadonlyIndex");
    let cluster_dir = dir
        .path()
        .join(".klei/DoNotStarveTogether/ClusterDeleteReadonlyIndex");
    fs::create_dir_all(cluster_dir.join("Master")).unwrap();
    fs::write(cluster_dir.join("Master/keep.txt"), "keep").unwrap();
    fs::write(
        cluster_dir.join("level.json"),
        r#"{"levelList":[{"name":"森林","file":"Master"}]}"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(cluster_dir.join("level.json"))
        .unwrap()
        .permissions();
    permissions.set_mode(0o444);
    fs::set_permissions(cluster_dir.join("level.json"), permissions).unwrap();

    let rejected = send(
        &app,
        Method::DELETE,
        "/api/cluster/level?levelName=Master",
        None,
        Some(&cookie),
    )
    .await;

    assert_ne!(rejected.status(), StatusCode::OK);
    assert_eq!(
        fs::read_to_string(cluster_dir.join("Master/keep.txt")).unwrap(),
        "keep"
    );
    assert!(
        !cluster_dir.read_dir().unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("__dst_admin_delete_")),
        "failed index writes must restore the staged level name"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn level_delete_restores_index_and_directory_when_tombstone_delete_fails() {
    use std::os::unix::fs::PermissionsExt;

    let (app, dir) = test_router().await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterDeleteRollback");
    let cluster_dir = dir
        .path()
        .join(".klei/DoNotStarveTogether/ClusterDeleteRollback");
    let locked_dir = cluster_dir.join("Master/locked");
    fs::create_dir_all(&locked_dir).unwrap();
    fs::write(locked_dir.join("keep.txt"), "keep").unwrap();
    fs::write(
        cluster_dir.join("level.json"),
        r#"{"levelList":[{"name":"森林","file":"Master"}]}"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&locked_dir).unwrap().permissions();
    permissions.set_mode(0o555);
    fs::set_permissions(&locked_dir, permissions).unwrap();

    let rejected = send(
        &app,
        Method::DELETE,
        "/api/cluster/level?levelName=Master",
        None,
        Some(&cookie),
    )
    .await;

    let mut permissions = fs::metadata(&locked_dir).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&locked_dir, permissions).unwrap();
    assert_ne!(rejected.status(), StatusCode::OK);
    assert_eq!(
        fs::read_to_string(locked_dir.join("keep.txt")).unwrap(),
        "keep"
    );
    assert_eq!(
        fs::read_to_string(cluster_dir.join("level.json")).unwrap(),
        r#"{"levelList":[{"name":"森林","file":"Master"}]}"#
    );
}

#[tokio::test]
async fn player_list_routes_preserve_line_based_files_and_reject_traversal() {
    let (app, dir) = test_router().await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterLists");

    let body = json!({"adminList": ["KU_admin", "KU_mod"]});
    let saved = send(
        &app,
        Method::POST,
        "/api/game/player/adminlist",
        Some(body),
        Some(&cookie),
    )
    .await;
    assert_eq!(saved.status(), StatusCode::OK);

    let adminlist = send(
        &app,
        Method::GET,
        "/api/game/player/adminlist",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(
        response_json(adminlist).await,
        json!({"code": 200, "msg": "success", "data": ["KU_admin", "KU_mod"]})
    );
    assert_eq!(
        fs::read_to_string(
            dir.path()
                .join(".klei/DoNotStarveTogether/ClusterLists/adminlist.txt")
        )
        .unwrap(),
        "KU_admin\nKU_mod\n"
    );

    let overwritten = send(
        &app,
        Method::POST,
        "/api/game/8level/adminilist",
        Some(json!({"adminList": ["KU_owner"]})),
        Some(&cookie),
    )
    .await;
    assert_eq!(overwritten.status(), StatusCode::OK);
    let adminlist = send(
        &app,
        Method::GET,
        "/api/game/8level/adminilist",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(response_json(adminlist).await["data"], json!(["KU_owner"]));

    let rejected = send(
        &app,
        Method::GET,
        "/api/game/8level/players?levelName=../ClusterLists",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_dst_write_targets_are_rejected_without_overwriting_escape() {
    use std::os::unix::fs::symlink;

    let (app, dir) = test_router().await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterSymlink");
    let cluster_dir = dir.path().join(".klei/DoNotStarveTogether/ClusterSymlink");
    fs::create_dir_all(&cluster_dir).unwrap();

    let escaped_adminlist = dir.path().join("escaped-adminlist.txt");
    fs::write(&escaped_adminlist, "keep\n").unwrap();
    symlink(&escaped_adminlist, cluster_dir.join("adminlist.txt")).unwrap();
    let rejected_adminlist = send(
        &app,
        Method::POST,
        "/api/game/player/adminlist",
        Some(json!({"adminList": ["KU_escape"]})),
        Some(&cookie),
    )
    .await;
    assert_eq!(rejected_adminlist.status(), StatusCode::BAD_REQUEST);
    assert_eq!(fs::read_to_string(&escaped_adminlist).unwrap(), "keep\n");

    let escaped_level_dir = dir.path().join("escaped-master");
    fs::create_dir_all(&escaped_level_dir).unwrap();
    fs::write(
        escaped_level_dir.join("modoverrides.lua"),
        "return { keep = true }",
    )
    .unwrap();
    symlink(&escaped_level_dir, cluster_dir.join("Master")).unwrap();
    fs::write(
        cluster_dir.join("level.json"),
        r#"{"levelList":[{"name":"森林","file":"Master"}]}"#,
    )
    .unwrap();

    let rejected_mod = send(
        &app,
        Method::POST,
        "/api/game/config",
        Some(json!({"modData": "return { escaped = true }"})),
        Some(&cookie),
    )
    .await;
    assert_eq!(rejected_mod.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        fs::read_to_string(escaped_level_dir.join("modoverrides.lua")).unwrap(),
        "return { keep = true }"
    );

    fs::write(
        escaped_level_dir.join("server.ini"),
        "[NETWORK]\nserver_port = 12000\n",
    )
    .unwrap();
    fs::remove_file(escaped_level_dir.join("leveldataoverride.lua")).ok();
    let rejected_read = send(&app, Method::GET, "/api/cluster/level", None, Some(&cookie)).await;
    assert_eq!(rejected_read.status(), StatusCode::BAD_REQUEST);
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_cluster_directory_does_not_create_external_level_directory() {
    use std::os::unix::fs::symlink;

    let (app, dir) = test_router().await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterDirLink");
    let klei_root = dir.path().join(".klei/DoNotStarveTogether");
    let cluster_link = klei_root.join("ClusterDirLink");
    let escaped_cluster = dir.path().join("escaped-cluster");
    fs::create_dir_all(&klei_root).unwrap();
    fs::create_dir_all(&escaped_cluster).unwrap();
    symlink(&escaped_cluster, &cluster_link).unwrap();

    let rejected = send(
        &app,
        Method::POST,
        "/api/cluster/level",
        Some(json!({
            "levelName": "External",
            "is_master": false,
            "uuid": "ExternalLevel",
            "leveldataoverride": "return {}",
            "modoverrides": "return {}",
            "server_ini": {
                "server_port": 12001,
                "is_master": false,
                "name": "ExternalLevel",
                "id": 10012,
                "encode_user_path": true,
                "authentication_port": 8768,
                "master_server_port": 27018
            }
        })),
        Some(&cookie),
    )
    .await;
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert!(!escaped_cluster.join("ExternalLevel").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn level_delete_rejects_symlinked_level_directory_without_deleting_escape_target() {
    use std::os::unix::fs::symlink;

    let (app, dir) = test_router().await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterDeleteLink");
    let cluster_dir = dir
        .path()
        .join(".klei/DoNotStarveTogether/ClusterDeleteLink");
    let escaped_level = dir.path().join("escaped-delete-level");
    fs::create_dir_all(&cluster_dir).unwrap();
    fs::create_dir_all(&escaped_level).unwrap();
    fs::write(escaped_level.join("keep.txt"), "keep").unwrap();
    symlink(&escaped_level, cluster_dir.join("Master")).unwrap();
    fs::write(
        cluster_dir.join("level.json"),
        r#"{"levelList":[{"name":"森林","file":"Master"}]}"#,
    )
    .unwrap();

    let rejected = send(
        &app,
        Method::DELETE,
        "/api/cluster/level?levelName=Master",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        fs::read_to_string(escaped_level.join("keep.txt")).unwrap(),
        "keep"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_level_directory_with_missing_files_is_rejected_on_read() {
    use std::os::unix::fs::symlink;

    let (app, dir) = test_router().await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterReadLink");
    let cluster_dir = dir.path().join(".klei/DoNotStarveTogether/ClusterReadLink");
    let empty_external_level = dir.path().join("empty-external-level");
    fs::create_dir_all(&cluster_dir).unwrap();
    fs::create_dir_all(&empty_external_level).unwrap();
    symlink(&empty_external_level, cluster_dir.join("Master")).unwrap();
    fs::write(
        cluster_dir.join("level.json"),
        r#"{"levelList":[{"name":"森林","file":"Master"}]}"#,
    )
    .unwrap();

    let rejected = send(&app, Method::GET, "/api/cluster/level", None, Some(&cookie)).await;
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn dst_and_game_config_routes_preserve_key_value_and_lua_files() {
    let (app, dir) = test_router().await;
    let cookie = login(&app).await;

    let saved_dst = send(
        &app,
        Method::POST,
        "/api/dst/config",
        Some(json!({
            "steamcmd": "/steam",
            "force_install_dir": dir.path().join("dst-server").display().to_string(),
            "donot_starve_server_directory": "",
            "cluster": "ClusterConfig",
            "backup": dir.path().join("backup").display().to_string(),
            "mod_download_path": dir.path().join("mods").display().to_string(),
            "bin": 64,
            "beta": 0,
            "ugc_directory": "",
            "persistent_storage_root": dir.path().display().to_string(),
            "conf_dir": "DoNotStarveTogether"
        })),
        Some(&cookie),
    )
    .await;
    assert_eq!(saved_dst.status(), StatusCode::OK);
    assert_eq!(
        response_json(saved_dst).await,
        json!({"code": 200, "msg": "save dst_config success", "data": null})
    );
    assert!(
        fs::read_to_string(dir.path().join("dst_config"))
            .unwrap()
            .contains("cluster=ClusterConfig")
    );

    let dst_config_before_rejected_save =
        fs::read_to_string(dir.path().join("dst_config")).unwrap();
    let rejected_dst_config = send(
        &app,
        Method::POST,
        "/api/dst/config",
        Some(json!({
            "steamcmd": "/steam",
            "force_install_dir": dir.path().join("dst-server").display().to_string(),
            "donot_starve_server_directory": "",
            "cluster": "../escape",
            "backup": dir.path().join("backup").display().to_string(),
            "mod_download_path": dir.path().join("mods").display().to_string(),
            "bin": 64,
            "beta": 0,
            "ugc_directory": "",
            "persistent_storage_root": dir.path().display().to_string(),
            "conf_dir": "DoNotStarveTogether"
        })),
        Some(&cookie),
    )
    .await;
    assert_eq!(rejected_dst_config.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        fs::read_to_string(dir.path().join("dst_config")).unwrap(),
        dst_config_before_rejected_save
    );

    fs::create_dir_all(dir.path().join("DoNotStarveTogether/ClusterConfig/Master")).unwrap();
    fs::write(
        dir.path()
            .join("DoNotStarveTogether/ClusterConfig/Master/modoverrides.lua"),
        "return {}",
    )
    .unwrap();
    fs::write(
        dir.path()
            .join("DoNotStarveTogether/ClusterConfig/cluster_token.txt"),
        "token",
    )
    .unwrap();
    fs::write(
        dir.path().join("DoNotStarveTogether/ClusterConfig/cluster.ini"),
        "[NETWORK]\ncluster_name = ClusterConfig\ncluster_description = config route\n[GAMEPLAY]\nmax_players = 6\n",
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("DoNotStarveTogether/ClusterConfig/Caves")).unwrap();
    fs::write(
        dir.path()
            .join("DoNotStarveTogether/ClusterConfig/Caves/modoverrides.lua"),
        "return {}",
    )
    .unwrap();

    let saved_game = send(
        &app,
        Method::POST,
        "/api/game/config",
        Some(json!({"modData": "return { [\"workshop-456\"] = { enabled = true } }"})),
        Some(&cookie),
    )
    .await;
    assert_eq!(saved_game.status(), StatusCode::OK);
    assert_eq!(
        response_json(saved_game).await["msg"],
        "save dst server config success"
    );
    assert_eq!(
        fs::read_to_string(
            dir.path()
                .join("DoNotStarveTogether/ClusterConfig/Master/modoverrides.lua")
        )
        .unwrap(),
        "return { [\"workshop-456\"] = { enabled = true } }"
    );
    assert_eq!(
        fs::read_to_string(
            dir.path()
                .join("DoNotStarveTogether/ClusterConfig/Caves/modoverrides.lua")
        )
        .unwrap(),
        "return { [\"workshop-456\"] = { enabled = true } }"
    );
    assert_eq!(
        fs::read_to_string(
            dir.path()
                .join("dst-server/mods/dedicated_server_mods_setup.lua")
        )
        .unwrap(),
        "ServerModSetup(\"456\")\n"
    );

    fs::write(
        dir.path()
            .join("DoNotStarveTogether/ClusterConfig/Master/modoverrides.lua"),
        "return { keep = true }",
    )
    .unwrap();
    let missing_mod_data = send(
        &app,
        Method::POST,
        "/api/game/config",
        Some(json!({})),
        Some(&cookie),
    )
    .await;
    assert_eq!(missing_mod_data.status(), StatusCode::OK);
    assert_eq!(
        fs::read_to_string(
            dir.path()
                .join("DoNotStarveTogether/ClusterConfig/Master/modoverrides.lua")
        )
        .unwrap(),
        "return { keep = true }"
    );

    let game_config = send(&app, Method::GET, "/api/game/config", None, Some(&cookie)).await;
    let body = response_json(game_config).await;
    assert_eq!(body["data"]["clusterName"], "ClusterConfig");
    assert_eq!(body["data"]["modData"], "return { keep = true }");

    fs::write(
        dir.path()
            .join("DoNotStarveTogether/ClusterConfig/level.json"),
        r#"{"levelList":[{"name":"bad","file":"../escape"}]}"#,
    )
    .unwrap();
    let rejected = send(
        &app,
        Method::POST,
        "/api/game/config",
        Some(json!({"modData": "return {}"})),
        Some(&cookie),
    )
    .await;
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);

    fs::write(
        dir.path()
            .join("DoNotStarveTogether/ClusterConfig/level.json"),
        "{broken",
    )
    .unwrap();
    fs::write(
        dir.path()
            .join("DoNotStarveTogether/ClusterConfig/Master/modoverrides.lua"),
        "return { keep = true }",
    )
    .unwrap();
    let malformed_index = send(
        &app,
        Method::POST,
        "/api/game/config",
        Some(json!({"modData": "return { broken = false }"})),
        Some(&cookie),
    )
    .await;
    assert_eq!(malformed_index.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        fs::read_to_string(
            dir.path()
                .join("DoNotStarveTogether/ClusterConfig/Master/modoverrides.lua")
        )
        .unwrap(),
        "return { keep = true }"
    );
}

async fn test_router() -> (Router, TempDir) {
    test_router_with_command_runner(FakeCommandRunner::default()).await
}

async fn test_router_with_command_runner<R>(command_runner: R) -> (Router, TempDir)
where
    R: CommandRunner + 'static,
{
    let dir = tempdir().unwrap();
    write_password_file(dir.path());
    let pool = connect_sqlite_memory().await.unwrap();
    migrate(&pool).await.unwrap();
    let state = AppState::new_with_command_runner(
        test_config(),
        pool,
        SessionStore::new(),
        dir.path(),
        command_runner,
    );
    (build_router(state), dir)
}

async fn test_router_with_processes_and_http(
    processes: FakeProcessSnapshotProvider,
    http_responses: Vec<HttpResponse>,
) -> (Router, TempDir, FakeHttpClient) {
    let dir = tempdir().unwrap();
    write_password_file(dir.path());
    let pool = connect_sqlite_memory().await.unwrap();
    migrate(&pool).await.unwrap();
    let http = FakeHttpClient::new(http_responses);
    let state = AppState::new_with_command_runner_and_http_client(
        test_config(),
        pool,
        SessionStore::new(),
        dir.path(),
        FakeCommandRunner::default(),
        http.clone(),
        processes,
    );
    (build_router(state), dir, http)
}

#[derive(Clone, Debug, Default)]
struct FakeProcessSnapshotProvider {
    snapshots: Vec<ProcessSnapshot>,
    calls: Arc<AtomicUsize>,
}

impl FakeProcessSnapshotProvider {
    fn new(snapshots: Vec<ProcessSnapshot>) -> Self {
        Self {
            snapshots,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ProcessSnapshotProvider for FakeProcessSnapshotProvider {
    fn snapshots(&self) -> std::io::Result<Vec<ProcessSnapshot>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.snapshots.clone())
    }
}

fn process_snapshot(cluster_name: &str, level_name: &str) -> ProcessSnapshot {
    ProcessSnapshot {
        pid: Some(1000),
        cpu_usage: "0.1".to_owned(),
        mem_usage: "0.2".to_owned(),
        virtual_size: "10".to_owned(),
        resident_set_size: "20".to_owned(),
        command: format!(
            "/srv/dst/bin/dontstarve_dedicated_server_nullrenderer -console -cluster {cluster_name} -shard {level_name}"
        ),
    }
}

async fn seed_cluster_row(app: &Router, root: &Path, cookie: &str, cluster_name: &str) {
    write_dst_config(root, cluster_name);
    fs::create_dir_all(root.join("server")).unwrap();

    let created = send(
        app,
        Method::POST,
        "/api/cluster",
        Some(json!({
            "clusterName": cluster_name,
            "description": "runtime fields",
            "steamcmd": "/opt/steamcmd",
            "force_install_dir": root.join("server").display().to_string(),
            "backup": root.join("backup").display().to_string(),
            "mod_download_path": root.join("mods").display().to_string(),
            "uuid": "",
            "beta": 0,
            "bin": 64,
            "ugc_directory": "",
            "persistent_storage_root": "",
            "conf_dir": ""
        })),
        Some(cookie),
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
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

fn write_password_file(root: &Path) {
    fs::write(
        root.join("password.txt"),
        "username=admin\npassword=123456\ndisplayName=Admin\nphotoURL=https://example.test/avatar.png\n",
    )
    .unwrap();
}

fn write_dst_config(root: &Path, cluster: &str) {
    fs::write(
        root.join("dst_config"),
        format!(
            "steamcmd=/steam\nforce_install_dir={}\ncluster={cluster}\nbackup={}\nmod_download_path={}\nbin=64\nbeta=0\n",
            root.join("server").display(),
            root.join("backup").display(),
            root.join("mods").display()
        ),
    )
    .unwrap();
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
