use std::{
    collections::VecDeque,
    fs, io,
    net::UdpSocket,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
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
    infra::command::{CommandOutput, CommandSpec, FakeCommandRunner},
    infra::config::{AppConfig, AutoUpdateModinfoConfig},
    infra::db::{connect_sqlite_memory, migrate},
    infra::process::{ProcessSnapshot, ProcessSnapshotProvider},
    web::app::{AppState, build_router},
};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};
use tower::ServiceExt;
use zip::ZipArchive;

#[tokio::test]
async fn start_and_stop_level_routes_use_safe_argv_and_legacy_messages() {
    let runner = FakeCommandRunner::new(vec![
        CommandOutput::success(Vec::new(), Vec::new()),
        CommandOutput::success(Vec::new(), Vec::new()),
        CommandOutput::success(Vec::new(), Vec::new()),
    ]);
    let (app, dir, runner) = test_router(runner).await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterLife", 64);
    write_level_fixture(dir.path(), "ClusterLife");
    let steam_source = dir.path().join("steamcmd/linux32/steamclient.so");
    fs::create_dir_all(steam_source.parent().unwrap()).unwrap();
    fs::write(&steam_source, "new-steamclient").unwrap();
    let steam_target = dir
        .path()
        .join("dst-dedicated-server/bin/lib32/steamclient.so");
    fs::create_dir_all(steam_target.parent().unwrap()).unwrap();
    fs::write(&steam_target, "old-steamclient").unwrap();

    let started = send(
        &app,
        Method::GET,
        "/api/game/8level/start?levelName=Master",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(started.status(), StatusCode::OK);
    assert_eq!(
        response_json(started).await,
        json!({"code": 200, "msg": "start ClusterLife Master success", "data": null})
    );
    assert_eq!(
        fs::read_to_string(&steam_target).unwrap(),
        "new-steamclient"
    );
    assert_eq!(
        fs::read_to_string(steam_target.with_file_name("steamclient.so.bak")).unwrap(),
        "old-steamclient"
    );

    let calls = runner.calls();
    assert_eq!(calls.len(), 2);
    assert_screen_call(
        &calls[0],
        "DST_8level_ClusterLife_Master",
        "c_shutdown(true)\n",
    );
    assert_eq!(calls[1].program(), "screen");
    assert_eq!(
        calls[1].current_dir(),
        Some(dir.path().join("dst-dedicated-server/bin64").as_path())
    );
    assert!(calls[1].args().contains(&"-d".to_owned()));
    assert!(calls[1].args().contains(&"-m".to_owned()));
    assert!(
        calls[1]
            .args()
            .contains(&"DST_8level_ClusterLife_Master".to_owned())
    );
    assert!(
        calls[1]
            .args()
            .iter()
            .any(|arg| arg == "./dontstarve_dedicated_server_nullrenderer_x64")
    );
    assert!(
        calls[1]
            .args()
            .windows(2)
            .any(|args| args == ["-cluster", "ClusterLife"])
    );
    assert!(
        calls[1]
            .args()
            .windows(2)
            .any(|args| args == ["-shard", "Master"])
    );
    assert_no_shell(&calls[1]);

    let stopped = send(
        &app,
        Method::GET,
        "/api/game/8level/stop?levelName=Master",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(stopped.status(), StatusCode::OK);
    assert_eq!(
        response_json(stopped).await,
        json!({"code": 200, "msg": "start ClusterLife Master success", "data": null})
    );
    let calls = runner.calls();
    assert_eq!(calls.len(), 3);
    assert_screen_call(
        &calls[2],
        "DST_8level_ClusterLife_Master",
        "c_shutdown(true)\n",
    );
}

#[tokio::test]
async fn start_level_repairs_missing_server_ini_before_launch() {
    let runner = FakeCommandRunner::new(vec![
        CommandOutput::success(Vec::new(), Vec::new()),
        CommandOutput::success(Vec::new(), Vec::new()),
    ]);
    let (app, dir, runner) = test_router(runner).await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterRepair", 64);
    write_level_fixture(dir.path(), "ClusterRepair");
    let cluster_dir = dir
        .path()
        .join(".klei/DoNotStarveTogether")
        .join("ClusterRepair");
    fs::write(
        cluster_dir.join("cluster.ini"),
        "[GAMEPLAY]\ngame_mode = \n[NETWORK]\ncluster_name = Keep Me\n",
    )
    .unwrap();
    fs::remove_file(cluster_dir.join("Caves/server.ini")).unwrap();

    let started = send(
        &app,
        Method::GET,
        "/api/game/8level/start?levelName=Caves",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(started.status(), StatusCode::OK);

    let repaired = fs::read_to_string(cluster_dir.join("Caves/server.ini")).unwrap();
    assert!(repaired.contains("server_port = 10998"));
    assert!(repaired.contains("is_master = false"));
    assert!(repaired.contains("name = Caves"));
    assert!(
        fs::read_to_string(cluster_dir.join("Caves/leveldataoverride.lua"))
            .unwrap()
            .contains("id = \"DST_CAVE\"")
    );
    let repaired_cluster = fs::read_to_string(cluster_dir.join("cluster.ini")).unwrap();
    assert!(repaired_cluster.contains("game_mode = survival"));
    assert!(repaired_cluster.contains("cluster_name = Keep Me"));

    let calls = runner.calls();
    assert_eq!(calls.len(), 2);
    assert_screen_call(
        &calls[0],
        "DST_8level_ClusterRepair_Caves",
        "c_shutdown(true)\n",
    );
    assert!(
        calls[1]
            .args()
            .windows(2)
            .any(|args| args == ["-shard", "Caves"])
    );
}

#[tokio::test]
async fn start_all_and_stop_all_follow_level_index_order() {
    let runner = FakeCommandRunner::new(vec![
        CommandOutput::success(Vec::new(), Vec::new()),
        CommandOutput::success(Vec::new(), Vec::new()),
        CommandOutput::success(Vec::new(), Vec::new()),
        CommandOutput::success(Vec::new(), Vec::new()),
        CommandOutput::success(Vec::new(), Vec::new()),
        CommandOutput::success(Vec::new(), Vec::new()),
    ]);
    let (app, dir, runner) = test_router(runner).await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterAll", 100);
    write_level_fixture(dir.path(), "ClusterAll");

    let started = send(
        &app,
        Method::GET,
        "/api/game/8level/start/all",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(started.status(), StatusCode::OK);
    assert_eq!(response_json(started).await["msg"], "start all success");

    let stopped = send(
        &app,
        Method::GET,
        "/api/game/8level/stop/all",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(stopped.status(), StatusCode::OK);
    assert_eq!(response_json(stopped).await["msg"], "stop all success");

    let calls = runner.calls();
    assert_eq!(calls.len(), 6);
    assert_screen_call(
        &calls[0],
        "DST_8level_ClusterAll_Master",
        "c_shutdown(true)\n",
    );
    assert_screen_call(
        &calls[1],
        "DST_8level_ClusterAll_Caves",
        "c_shutdown(true)\n",
    );
    assert!(
        calls[2]
            .args()
            .iter()
            .any(|arg| arg == "./dontstarve_dedicated_server_nullrenderer_x64_luajit")
    );
    assert!(
        calls[2]
            .args()
            .windows(2)
            .any(|args| args == ["-shard", "Master"])
    );
    assert!(
        calls[3]
            .args()
            .windows(2)
            .any(|args| args == ["-shard", "Caves"])
    );
    assert_screen_call(
        &calls[4],
        "DST_8level_ClusterAll_Master",
        "c_shutdown(true)\n",
    );
    assert_screen_call(
        &calls[5],
        "DST_8level_ClusterAll_Caves",
        "c_shutdown(true)\n",
    );
}

#[tokio::test]
async fn update_game_route_stops_levels_then_runs_steamcmd_update_argv() {
    let runner = FakeCommandRunner::new(vec![
        CommandOutput::success(Vec::new(), Vec::new()),
        CommandOutput::success(Vec::new(), Vec::new()),
        CommandOutput::success(Vec::new(), Vec::new()),
    ]);
    let (app, dir, runner) = test_router(runner).await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterUpdate", 64);
    write_level_fixture(dir.path(), "ClusterUpdate");
    let cluster_dir = dir.path().join(".klei/DoNotStarveTogether/ClusterUpdate");
    fs::write(
        cluster_dir.join("Master/modoverrides.lua"),
        "return { [\"workshop-123\"] = { enabled = true } }",
    )
    .unwrap();
    fs::write(
        cluster_dir.join("Caves/modoverrides.lua"),
        "return { [\"workshop-456\"] = { enabled = true } }",
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("steamcmd")).unwrap();
    let mods_setup = dir
        .path()
        .join("dst-dedicated-server/mods/dedicated_server_mods_setup.lua");
    fs::create_dir_all(mods_setup.parent().unwrap()).unwrap();
    fs::write(&mods_setup, "ServerModSetup(\"999\")\n").unwrap();

    let response = send(&app, Method::GET, "/api/game/update", None, Some(&cookie)).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_json(response).await,
        json!({"code": 200, "msg": "update dst success", "data": null})
    );
    let calls = runner.calls();
    assert_eq!(calls.len(), 3);
    assert_screen_call(
        &calls[0],
        "DST_8level_ClusterUpdate_Master",
        "c_shutdown(true)\n",
    );
    assert_screen_call(
        &calls[1],
        "DST_8level_ClusterUpdate_Caves",
        "c_shutdown(true)\n",
    );
    assert!(calls[2].program().ends_with(if cfg!(windows) {
        "steamcmd.exe"
    } else {
        "steamcmd"
    }));
    assert_eq!(
        calls[2].current_dir(),
        Some(dir.path().join("steamcmd").as_path())
    );
    assert!(
        calls[2]
            .args()
            .windows(2)
            .any(|args| args == ["+login", "anonymous"])
    );
    assert!(
        calls[2]
            .args()
            .windows(2)
            .any(|args| args == ["+app_update", "343050"])
    );
    assert!(calls[2].args().contains(&"validate".to_owned()));
    assert_no_shell(&calls[2]);
    assert_eq!(
        fs::read_to_string(mods_setup).unwrap(),
        "ServerModSetup(\"456\")\nServerModSetup(\"123\")\nServerModSetup(\"999\")\n"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn update_game_bin_2664_uses_depot_downloader_without_stopping_levels() {
    let runner = FakeCommandRunner::new(vec![CommandOutput::success(Vec::new(), Vec::new())]);
    let (app, dir, runner) = test_router(runner).await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterDepot", 2664);
    write_level_fixture(dir.path(), "ClusterDepot");

    let response = send(&app, Method::GET, "/api/game/update", None, Some(&cookie)).await;

    assert_eq!(response.status(), StatusCode::OK);
    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].program(), "./DepotDownloader");
    assert_eq!(
        calls[0].current_dir(),
        Some(Path::new("/opt/DepotDownloader"))
    );
    assert!(
        calls[0]
            .args()
            .windows(2)
            .any(|args| args == ["-app", "343050"])
    );
    assert!(
        calls[0].args().windows(2).any(|args| args
            == [
                "-dir",
                dir.path().join("dst-dedicated-server").to_str().unwrap()
            ]),
        "DepotDownloader should install to the configured force_install_dir"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn update_game_bin_2664_rejects_running_levels_without_depotdownloader() {
    let runner = FakeCommandRunner::default();
    let matching_process = ProcessSnapshot {
        pid: Some(66_001),
        cpu_usage: "0.1".to_owned(),
        mem_usage: "0.2".to_owned(),
        virtual_size: "10".to_owned(),
        resident_set_size: "20".to_owned(),
        command: "/dst/bin/dontstarve_dedicated_server_nullrenderer_x64 -console -cluster ClusterDepotRunning -shard Master"
            .to_owned(),
    };
    let (app, dir, runner) = test_router_with_processes(runner, vec![matching_process]).await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterDepotRunning", 2664);
    write_level_fixture(dir.path(), "ClusterDepotRunning");

    let response = send(&app, Method::GET, "/api/game/update", None, Some(&cookie)).await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(runner.calls().is_empty());
}

#[tokio::test]
async fn update_game_does_not_run_steamcmd_when_stop_barrier_still_sees_process() {
    let runner = FakeCommandRunner::new(vec![
        CommandOutput::success(Vec::new(), Vec::new()),
        CommandOutput::success(Vec::new(), Vec::new()),
    ]);
    let matching_process = ProcessSnapshot {
        pid: Some(77_001),
        cpu_usage: "0.1".to_owned(),
        mem_usage: "0.2".to_owned(),
        virtual_size: "10".to_owned(),
        resident_set_size: "20".to_owned(),
        command: "/dst/bin/dontstarve_dedicated_server_nullrenderer -console -cluster ClusterStrictUpdate -shard Master"
            .to_owned(),
    };
    let (app, dir, runner) = test_router_with_process_sequences(
        runner,
        vec![vec![matching_process.clone()], vec![matching_process]],
        Duration::ZERO,
    )
    .await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterStrictUpdate", 64);
    write_level_fixture(dir.path(), "ClusterStrictUpdate");
    fs::create_dir_all(dir.path().join("steamcmd")).unwrap();

    let response = send(&app, Method::GET, "/api/game/update", None, Some(&cookie)).await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let calls = runner.calls();
    assert_eq!(calls.len(), 2);
    assert_screen_call(
        &calls[0],
        "DST_8level_ClusterStrictUpdate_Master",
        "c_shutdown(true)\n",
    );
    assert_eq!(calls[1].program(), "kill");
    assert!(
        !calls
            .iter()
            .any(|call| call.program().ends_with("steamcmd"))
    );
}

#[tokio::test]
async fn update_game_does_not_rewrite_mod_setup_when_steamcmd_fails() {
    let runner = FakeCommandRunner::new(vec![
        CommandOutput::success(Vec::new(), Vec::new()),
        CommandOutput::success(Vec::new(), Vec::new()),
        command_output_status(1),
    ]);
    let (app, dir, _runner) = test_router(runner).await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterUpdateFail", 64);
    write_level_fixture(dir.path(), "ClusterUpdateFail");
    let cluster_dir = dir
        .path()
        .join(".klei/DoNotStarveTogether/ClusterUpdateFail");
    fs::write(
        cluster_dir.join("Master/modoverrides.lua"),
        "return { [\"workshop-123\"] = { enabled = true } }",
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("steamcmd")).unwrap();
    let mods_setup = dir
        .path()
        .join("dst-dedicated-server/mods/dedicated_server_mods_setup.lua");
    fs::create_dir_all(mods_setup.parent().unwrap()).unwrap();
    fs::write(&mods_setup, "ServerModSetup(\"999\")\n").unwrap();

    let response = send(&app, Method::GET, "/api/game/update", None, Some(&cookie)).await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        fs::read_to_string(mods_setup).unwrap(),
        "ServerModSetup(\"999\")\n"
    );
}

#[tokio::test]
async fn stop_level_runs_safe_kill_fallback_for_matching_process() {
    let runner = FakeCommandRunner::new(vec![
        CommandOutput::success(Vec::new(), Vec::new()),
        CommandOutput::success(Vec::new(), Vec::new()),
    ]);
    let (app, dir, runner) = test_router_with_processes(
        runner,
        vec![ProcessSnapshot {
            pid: Some(12_345),
            cpu_usage: "0.1".to_owned(),
            mem_usage: "0.2".to_owned(),
            virtual_size: "10".to_owned(),
            resident_set_size: "20".to_owned(),
            command: "/dst/bin/dontstarve_dedicated_server_nullrenderer -console -cluster ClusterKillFallback -shard Master"
                .to_owned(),
        }],
    )
    .await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterKillFallback", 64);
    write_level_fixture(dir.path(), "ClusterKillFallback");

    let response = send(
        &app,
        Method::GET,
        "/api/game/8level/stop?levelName=Master",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let calls = runner.calls();
    assert_eq!(calls.len(), 2);
    assert_screen_call(
        &calls[0],
        "DST_8level_ClusterKillFallback_Master",
        "c_shutdown(true)\n",
    );
    assert_eq!(calls[1].program(), "kill");
    assert_eq!(calls[1].args(), ["-9", "12345"]);
    assert_no_shell(&calls[1]);
}

#[tokio::test]
async fn stop_level_waits_for_graceful_exit_before_hard_kill() {
    let runner = FakeCommandRunner::new(vec![CommandOutput::success(Vec::new(), Vec::new())]);
    let (app, dir, runner) =
        test_router_with_process_sequences(runner, vec![Vec::new()], Duration::ZERO).await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterGrace", 64);
    write_level_fixture(dir.path(), "ClusterGrace");

    let response = send(
        &app,
        Method::GET,
        "/api/game/8level/stop?levelName=Master",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    assert_screen_call(
        &calls[0],
        "DST_8level_ClusterGrace_Master",
        "c_shutdown(true)\n",
    );
}

#[tokio::test]
async fn stop_level_kills_only_the_matched_process_pid_after_grace_period() {
    let runner = FakeCommandRunner::new(vec![
        CommandOutput::success(Vec::new(), Vec::new()),
        CommandOutput::success(Vec::new(), Vec::new()),
    ]);
    let (app, dir, runner) = test_router_with_process_sequences(
        runner,
        vec![vec![ProcessSnapshot {
            pid: Some(54_321),
            cpu_usage: "0.1".to_owned(),
            mem_usage: "0.2".to_owned(),
            virtual_size: "10".to_owned(),
            resident_set_size: "20".to_owned(),
            command: "/dst/bin/dontstarve_dedicated_server_nullrenderer -console -cluster ClusterPidKill -shard Master"
                .to_owned(),
        }]],
        Duration::ZERO,
    )
    .await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterPidKill", 64);
    write_level_fixture(dir.path(), "ClusterPidKill");

    let response = send(
        &app,
        Method::GET,
        "/api/game/8level/stop?levelName=Master",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let calls = runner.calls();
    assert_eq!(calls.len(), 2);
    assert_screen_call(
        &calls[0],
        "DST_8level_ClusterPidKill_Master",
        "c_shutdown(true)\n",
    );
    assert_eq!(calls[1].program(), "kill");
    assert_eq!(calls[1].args(), ["-9", "54321"]);
    assert_no_shell(&calls[1]);
}

#[tokio::test]
async fn operate_player_preserves_legacy_missing_path_param_quirk() {
    let runner = FakeCommandRunner::new(vec![
        CommandOutput::success(Vec::new(), Vec::new()),
        CommandOutput::success(Vec::new(), Vec::new()),
    ]);
    let (app, dir, runner) = test_router(runner).await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterOperate", 64);

    let response = send(
        &app,
        Method::GET,
        "/api/game/operate/player?type=1&kuId=KU_admin-1",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_json(response).await["data"], Value::Null);
    let calls = runner.calls();
    assert_eq!(calls.len(), 2);
    assert_screen_call(&calls[0], "DST_8level_ClusterOperate_Master", "\n");
    assert_screen_call(&calls[1], "DST_8level_ClusterOperate_Master", "\n");
}

#[tokio::test]
async fn udp_port_scan_is_bounded_to_legacy_range() {
    let (app, _dir, _runner) = test_router(FakeCommandRunner::default()).await;
    let cookie = login(&app).await;
    let reserved = UdpSocket::bind("127.0.0.1:10998").ok();

    let response = send(
        &app,
        Method::GET,
        "/api/game/8level/udp/port",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    let ports = body["data"].as_array().unwrap();
    assert!(ports.iter().all(|port| {
        let port = port.as_i64().unwrap();
        (10998..=11038).contains(&port)
    }));
    if reserved.is_some() {
        assert!(!ports.contains(&json!(10998)));
    }
}

#[tokio::test]
async fn preinstall_replaces_cluster_directory_and_preserves_identity_files() {
    let (app, dir, runner) = test_router(FakeCommandRunner::default()).await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterPreinstall", 64);
    write_level_fixture(dir.path(), "ClusterPreinstall");
    let cluster_dir = dir
        .path()
        .join(".klei/DoNotStarveTogether/ClusterPreinstall");
    fs::write(cluster_dir.join("cluster_token.txt"), "keep-token").unwrap();
    fs::write(cluster_dir.join("adminlist.txt"), "KU_admin\n").unwrap();
    fs::write(cluster_dir.join("old-only.txt"), "removed").unwrap();
    let template = dir.path().join("static/preinstall/default");
    fs::create_dir_all(template.join("Master")).unwrap();
    fs::write(
        template.join("cluster.ini"),
        "[GAMEPLAY]\ngame_mode = endless\n",
    )
    .unwrap();
    fs::write(
        template.join("Master/server.ini"),
        "[SHARD]\nis_master = true\n",
    )
    .unwrap();

    let response = send(
        &app,
        Method::GET,
        "/api/game/preinstall?name=default",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_json(response).await,
        json!({"code": 200, "msg": "success", "data": null})
    );
    assert_eq!(
        fs::read_to_string(cluster_dir.join("cluster_token.txt")).unwrap(),
        "keep-token"
    );
    assert_eq!(
        fs::read_to_string(cluster_dir.join("adminlist.txt")).unwrap(),
        "KU_admin\n"
    );
    assert_eq!(
        fs::read_to_string(cluster_dir.join("cluster.ini")).unwrap(),
        "[GAMEPLAY]\ngame_mode = endless\n"
    );
    assert!(!cluster_dir.join("old-only.txt").exists());
    let calls = runner.calls();
    assert_eq!(calls.len(), 3);
    assert_screen_call(
        &calls[0],
        "DST_8level_ClusterPreinstall_Master",
        "c_shutdown(true)\n",
    );
    assert_screen_call(
        &calls[1],
        "DST_8level_ClusterPreinstall_Caves",
        "c_shutdown(true)\n",
    );
    assert_screen_call(
        &calls[2],
        "DST_8level_ClusterPreinstall_Master",
        "c_save()\n",
    );

    let backup_files = fs::read_dir(dir.path().join("backup"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "zip"))
        .collect::<Vec<_>>();
    assert_eq!(backup_files.len(), 1);
    assert!(
        !backup_files[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("_preinstall.zip")
    );
    let mut archive = ZipArchive::new(fs::File::open(&backup_files[0]).unwrap()).unwrap();
    assert!(archive.by_name("ClusterPreinstall/old-only.txt").is_ok());
    assert!(
        archive
            .by_name("ClusterPreinstall/cluster_token.txt")
            .is_ok()
    );
}

#[tokio::test]
async fn preinstall_allows_cluster_named_bak_without_deleting_it() {
    let (app, dir, _runner) = test_router(FakeCommandRunner::default()).await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "bak", 64);
    write_level_fixture(dir.path(), "bak");
    let cluster_dir = dir.path().join(".klei/DoNotStarveTogether/bak");
    fs::write(cluster_dir.join("cluster_token.txt"), "still-here").unwrap();
    let template = dir.path().join("static/preinstall/default");
    fs::create_dir_all(template.join("Master")).unwrap();
    fs::write(template.join("cluster.ini"), "replacement").unwrap();

    let response = send(
        &app,
        Method::GET,
        "/api/game/preinstall?name=default",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        fs::read_to_string(cluster_dir.join("cluster_token.txt")).unwrap(),
        "still-here"
    );
    assert_eq!(
        fs::read_to_string(cluster_dir.join("cluster.ini")).unwrap(),
        "replacement"
    );
}

#[tokio::test]
async fn preinstall_rejects_existing_reserved_staging_directory() {
    let (app, dir, _runner) = test_router(FakeCommandRunner::default()).await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterStageConflict", 64);
    write_level_fixture(dir.path(), "ClusterStageConflict");
    let klei_root = dir.path().join(".klei/DoNotStarveTogether");
    let cluster_dir = klei_root.join("ClusterStageConflict");
    fs::write(cluster_dir.join("cluster_token.txt"), "still-here").unwrap();
    fs::create_dir(klei_root.join(".dst-admin-rust-preinstall-staging")).unwrap();
    let template = dir.path().join("static/preinstall/default");
    fs::create_dir_all(template.join("Master")).unwrap();
    fs::write(template.join("cluster.ini"), "replacement").unwrap();

    let response = send(
        &app,
        Method::GET,
        "/api/game/preinstall?name=default",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        fs::read_to_string(cluster_dir.join("cluster_token.txt")).unwrap(),
        "still-here"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn preinstall_rejects_zip_traversal_names_before_replacing_cluster() {
    let (app, dir, _runner) = test_router(FakeCommandRunner::default()).await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterZipEntry", 64);
    write_level_fixture(dir.path(), "ClusterZipEntry");
    let cluster_dir = dir.path().join(".klei/DoNotStarveTogether/ClusterZipEntry");
    fs::write(cluster_dir.join("..\\outside.txt"), "unsafe archive name").unwrap();
    fs::write(cluster_dir.join("cluster.ini"), "original").unwrap();
    let template = dir.path().join("static/preinstall/default");
    fs::create_dir_all(template.join("Master")).unwrap();
    fs::write(template.join("cluster.ini"), "replacement").unwrap();

    let response = send(
        &app,
        Method::GET,
        "/api/game/preinstall?name=default",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        fs::read_to_string(cluster_dir.join("cluster.ini")).unwrap(),
        "original"
    );
    assert!(cluster_dir.join("..\\outside.txt").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn preinstall_rejects_symlink_backup_entry_without_partial_zip() {
    let (app, dir, _runner) = test_router(FakeCommandRunner::default()).await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterZipSymlink", 64);
    write_level_fixture(dir.path(), "ClusterZipSymlink");
    let outside = dir.path().join("outside.txt");
    fs::write(&outside, "outside").unwrap();
    let cluster_dir = dir
        .path()
        .join(".klei/DoNotStarveTogether/ClusterZipSymlink");
    std::os::unix::fs::symlink(&outside, cluster_dir.join("escape-link")).unwrap();
    let template = dir.path().join("static/preinstall/default");
    fs::create_dir_all(template.join("Master")).unwrap();
    fs::write(template.join("cluster.ini"), "replacement").unwrap();

    let response = send(
        &app,
        Method::GET,
        "/api/game/preinstall?name=default",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!dir.path().join("backup").read_dir().unwrap().any(|entry| {
        entry
            .unwrap()
            .path()
            .extension()
            .is_some_and(|extension| extension == "zip")
    }));
    assert!(cluster_dir.join("escape-link").exists());
}

#[tokio::test]
async fn preinstall_does_not_replace_cluster_when_stop_barrier_still_sees_process() {
    let runner = FakeCommandRunner::new(vec![
        CommandOutput::success(Vec::new(), Vec::new()),
        CommandOutput::success(Vec::new(), Vec::new()),
    ]);
    let matching_process = ProcessSnapshot {
        pid: Some(88_001),
        cpu_usage: "0.1".to_owned(),
        mem_usage: "0.2".to_owned(),
        virtual_size: "10".to_owned(),
        resident_set_size: "20".to_owned(),
        command: "/dst/bin/dontstarve_dedicated_server_nullrenderer -console -cluster ClusterPreinstallStrict -shard Master"
            .to_owned(),
    };
    let (app, dir, runner) = test_router_with_process_sequences(
        runner,
        vec![vec![matching_process.clone()], vec![matching_process]],
        Duration::ZERO,
    )
    .await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterPreinstallStrict", 64);
    write_level_fixture(dir.path(), "ClusterPreinstallStrict");
    let cluster_dir = dir
        .path()
        .join(".klei/DoNotStarveTogether/ClusterPreinstallStrict");
    fs::write(cluster_dir.join("cluster.ini"), "original").unwrap();
    let template = dir.path().join("static/preinstall/default");
    fs::create_dir_all(template.join("Master")).unwrap();
    fs::write(template.join("cluster.ini"), "replacement").unwrap();

    let response = send(
        &app,
        Method::GET,
        "/api/game/preinstall?name=default",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        fs::read_to_string(cluster_dir.join("cluster.ini")).unwrap(),
        "original"
    );
    assert!(!dir.path().join("backup").read_dir().unwrap().any(|entry| {
        entry
            .unwrap()
            .path()
            .extension()
            .is_some_and(|extension| extension == "zip")
    }));
    assert_eq!(runner.calls().len(), 2);
}

#[tokio::test]
async fn lifecycle_routes_reject_invalid_level_before_command_construction() {
    let (app, dir, runner) = test_router(FakeCommandRunner::default()).await;
    let cookie = login(&app).await;
    write_dst_config(dir.path(), "ClusterReject", 64);
    write_level_fixture(dir.path(), "ClusterReject");

    let response = send(
        &app,
        Method::GET,
        "/api/game/8level/start?levelName=Master;touch-pwned",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(runner.calls().is_empty());
}

async fn test_router(runner: FakeCommandRunner) -> (Router, TempDir, FakeCommandRunner) {
    test_router_with_processes(runner, Vec::new()).await
}

async fn test_router_with_processes(
    runner: FakeCommandRunner,
    snapshots: Vec<ProcessSnapshot>,
) -> (Router, TempDir, FakeCommandRunner) {
    test_router_with_process_sequences(runner, vec![snapshots], Duration::ZERO).await
}

async fn test_router_with_process_sequences(
    runner: FakeCommandRunner,
    snapshot_sequences: Vec<Vec<ProcessSnapshot>>,
    lifecycle_grace_period: Duration,
) -> (Router, TempDir, FakeCommandRunner) {
    let dir = tempdir().unwrap();
    write_password_file(dir.path());
    let pool = connect_sqlite_memory().await.unwrap();
    migrate(&pool).await.unwrap();
    let state = AppState::new_with_command_runner_and_process_snapshot_provider(
        test_config(),
        pool,
        SessionStore::new(),
        dir.path(),
        runner.clone(),
        FakeProcessSnapshotProvider::new(snapshot_sequences),
    )
    .with_lifecycle_grace_period(lifecycle_grace_period);
    (build_router(state), dir, runner)
}

#[derive(Debug, Clone)]
struct FakeProcessSnapshotProvider {
    snapshots: Arc<Mutex<VecDeque<Vec<ProcessSnapshot>>>>,
}

impl FakeProcessSnapshotProvider {
    fn new(snapshots: Vec<Vec<ProcessSnapshot>>) -> Self {
        Self {
            snapshots: Arc::new(Mutex::new(snapshots.into())),
        }
    }
}

impl ProcessSnapshotProvider for FakeProcessSnapshotProvider {
    fn snapshots(&self) -> io::Result<Vec<ProcessSnapshot>> {
        let mut snapshots = self.snapshots.lock().expect("fake snapshots poisoned");
        if snapshots.len() > 1 {
            return Ok(snapshots.pop_front().unwrap());
        }
        Ok(snapshots.front().cloned().unwrap_or_default())
    }
}

fn assert_screen_call(spec: &CommandSpec, session: &str, command: &str) {
    assert_eq!(spec.program(), "screen");
    assert_eq!(
        spec.args(),
        ["-S", session, "-p", "0", "-X", "stuff", command]
    );
    assert_no_shell(spec);
}

fn assert_no_shell(spec: &CommandSpec) {
    assert_ne!(spec.program(), "sh");
    assert_ne!(spec.program(), "bash");
    assert!(!spec.args().iter().any(|arg| arg == "-c"));
    assert!(!spec.args().iter().any(|arg| arg.contains(" ; ")));
}

fn command_output_status(status_code: i32) -> CommandOutput {
    CommandOutput {
        status_code: Some(status_code),
        stdout: Vec::new(),
        stderr: Vec::new(),
        timed_out: false,
        stdout_truncated: false,
        stderr_truncated: false,
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

async fn send(
    app: &Router,
    method: Method,
    uri: &str,
    json_body: Option<Value>,
    cookie: Option<&str>,
) -> Response<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(cookie) = cookie {
        builder = builder.header(COOKIE, cookie);
    }
    let body = if let Some(value) = json_body {
        builder = builder.header(CONTENT_TYPE, "application/json");
        Body::from(serde_json::to_vec(&value).unwrap())
    } else {
        Body::empty()
    };
    app.clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap()
}

async fn response_json(response: Response<Body>) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
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

fn write_dst_config(root: &Path, cluster: &str, bin: i64) {
    fs::write(
        root.join("dst_config"),
        format!(
            "steamcmd={0}/steamcmd\nforce_install_dir={0}/dst-dedicated-server\ncluster={cluster}\nbackup={0}/backup\nmod_download_path={0}/mods\nbin={bin}\nbeta=0\n",
            root.display()
        ),
    )
    .unwrap();
}

fn write_level_fixture(root: &Path, cluster: &str) {
    let cluster_dir = root.join(".klei/DoNotStarveTogether").join(cluster);
    fs::create_dir_all(cluster_dir.join("Master")).unwrap();
    fs::create_dir_all(cluster_dir.join("Caves")).unwrap();
    fs::write(
        cluster_dir.join("level.json"),
        r#"{"levelList":[{"name":"森林","file":"Master"},{"name":"洞穴","file":"Caves"}]}"#,
    )
    .unwrap();
    for (level, is_master, port) in [("Master", true, 10999), ("Caves", false, 11000)] {
        fs::write(
            cluster_dir.join(level).join("leveldataoverride.lua"),
            "return {}",
        )
        .unwrap();
        fs::write(
            cluster_dir.join(level).join("modoverrides.lua"),
            "return {}",
        )
        .unwrap();
        fs::write(
            cluster_dir.join(level).join("server.ini"),
            format!(
                "[NETWORK]\nserver_port = {port}\n[SHARD]\nis_master = {is_master}\nname = {level}\nid = 10000\n[ACCOUNT]\nencode_user_path = true\n"
            ),
        )
        .unwrap();
    }
}
