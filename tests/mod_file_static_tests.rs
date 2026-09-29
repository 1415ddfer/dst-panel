use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
};

#[cfg(unix)]
use std::os::unix::fs::symlink;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{
        Method, Request, Response, StatusCode,
        header::{AUTHORIZATION, CONTENT_TYPE, COOKIE, SET_COOKIE},
    },
};
use dst_admin_rust::{
    domain::auth::SessionStore,
    infra::command::{CommandFuture, CommandOutput, CommandRunner, CommandSpec, FakeCommandRunner},
    infra::config::{AppConfig, AutoUpdateModinfoConfig},
    infra::db::{SqlitePool, connect_sqlite_memory, migrate},
    infra::http_client::{FakeHttpClient, HttpResponse},
    infra::process::SystemProcessSnapshotProvider,
    web::app::{AppState, build_router},
};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};
use tokio::time::{Duration, timeout};
use tower::ServiceExt;

#[tokio::test]
async fn modinfo_db_routes_round_trip_legacy_shapes() {
    let (app, dir, _pool, _http) = test_router(Vec::new()).await;
    let cookie = login(&app).await;

    let save = send_json(
        &app,
        Method::POST,
        "/api/mod/modinfo",
        json!({
            "auth": "https://steamcommunity.com/profiles/1/?xml=1",
            "consumer_appid": 322330.0,
            "creator_appid": 322330.0,
            "description": "local description",
            "file_url": "",
            "modid": "localmod",
            "img": "preview.png",
            "last_time": 12.0,
            "mod_config": "{\"enabled\":true}",
            "name": "Local Mod",
            "v": "1.0",
            "update": true
        }),
        Some(&cookie),
    )
    .await;
    assert_eq!(save.status(), StatusCode::OK);
    let save_body = response_json(save).await;
    assert_eq!(save_body["code"], 200);
    assert_eq!(save_body["msg"], "success");
    assert!(save_body["data"]["ID"].as_i64().unwrap() > 0);
    assert_eq!(save_body["data"]["modid"], "localmod");
    assert_eq!(save_body["data"]["mod_config"], "{\"enabled\":true}");

    let list = send(&app, Method::GET, "/api/mod", None, Some(&cookie)).await;
    assert_eq!(list.status(), StatusCode::OK);
    let list_body = response_json(list).await;
    assert_eq!(list_body["code"], 200);
    assert_eq!(list_body["data"][0]["consumer_id"], 322330.0);
    assert_eq!(list_body["data"][0]["mod_config"], json!({"enabled": true}));

    let raw = send(
        &app,
        Method::GET,
        "/api/mod/modinfo/localmod",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(raw.status(), StatusCode::OK);
    let raw_body = response_json(raw).await;
    assert_eq!(raw_body["data"]["mod_config"], "{\"enabled\":true}");

    let download_dir = dir
        .path()
        .join("mod-download/steamapps/workshop/content/322330/localmod");
    fs::create_dir_all(&download_dir).unwrap();
    fs::write(download_dir.join("modinfo.lua"), "name = \"Local Mod\"").unwrap();

    let delete = send(
        &app,
        Method::DELETE,
        "/api/mod/localmod",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(delete.status(), StatusCode::OK);
    assert_eq!(response_json(delete).await["data"], "localmod");
    assert!(!download_dir.exists());
}

#[tokio::test]
async fn manual_modinfo_file_writes_exact_lua_and_adds_local_record() {
    let (app, dir, _pool, _http) = test_router(Vec::new()).await;
    let cookie = login(&app).await;
    let lua = modinfo_lua("Manual Mod");

    let response = send_json(
        &app,
        Method::POST,
        "/api/mod/modinfo/file",
        json!({"workshopId": "manual-mod", "modinfo": lua}),
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_json(response).await,
        json!({"code": 200, "msg": "success", "data": null})
    );
    let modinfo_path = dir
        .path()
        .join("mod-download/steamapps/workshop/content/322330/manual-mod/modinfo.lua");
    assert_eq!(fs::read_to_string(modinfo_path).unwrap(), lua);

    let raw = send(
        &app,
        Method::GET,
        "/api/mod/modinfo/manual-mod",
        None,
        Some(&cookie),
    )
    .await;
    let raw_body = response_json(raw).await;
    assert_eq!(raw_body["data"]["modid"], "manual-mod");
    let raw_config: Value =
        serde_json::from_str(raw_body["data"]["mod_config"].as_str().unwrap()).unwrap();
    assert_eq!(raw_config["name"], "Manual Mod");
    assert_eq!(raw_config["api_version"], 10.0);
    assert_eq!(raw_config["dst_compatible"], true);
    assert_eq!(
        raw_config["configuration_options"][0]["options"][1]["description"],
        "Hard"
    );

    let detail = send(
        &app,
        Method::GET,
        "/api/mod/manual-mod",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(
        response_json(detail).await["data"]["mod_config"]["configuration_options"][0]["default"],
        "easy"
    );
}

#[tokio::test]
async fn manual_modinfo_file_applies_language_and_lua_globals_like_go() {
    let (app, _dir, _pool, _http) = test_router(Vec::new()).await;
    let cookie = login(&app).await;
    let lua = r#"
name = ChooseTranslationTable({ zh = "中文名称", en = "English Name", "Fallback Name" })
description = folder_name
locale_echo = locale
"#;

    let response = send_json(
        &app,
        Method::POST,
        "/api/mod/modinfo/file?lang=en",
        json!({"workshopId": "localized-mod", "modinfo": lua}),
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let raw = send(
        &app,
        Method::GET,
        "/api/mod/modinfo/localized-mod",
        None,
        Some(&cookie),
    )
    .await;
    let raw_body = response_json(raw).await;
    let raw_config: Value =
        serde_json::from_str(raw_body["data"]["mod_config"].as_str().unwrap()).unwrap();
    assert_eq!(raw_config["name"], "English Name");
    assert_eq!(raw_config["description"], "workshop-localized-mod");
    assert_eq!(raw_config["locale_echo"], "en");
}

#[tokio::test]
async fn numeric_manual_modinfo_file_does_not_create_duplicate_active_rows() {
    let (app, _dir, _pool, _http) =
        test_router(vec![steam_details_response("123456", "Numeric Manual")]).await;
    let cookie = login(&app).await;

    let response = send_json(
        &app,
        Method::POST,
        "/api/mod/modinfo/file",
        json!({"workshopId": "123456", "modinfo": "name = \"Numeric Manual\""}),
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let list = send(&app, Method::GET, "/api/mod", None, Some(&cookie)).await;
    let body = response_json(list).await;
    let rows = body["data"].as_array().unwrap();
    assert_eq!(
        rows.iter().filter(|row| row["modid"] == "123456").count(),
        1
    );
}

#[tokio::test]
async fn manual_modinfo_file_rejects_traversal_workshop_id() {
    let (app, dir, _pool, _http) = test_router(Vec::new()).await;
    let cookie = login(&app).await;

    let response = send_json(
        &app,
        Method::POST,
        "/api/mod/modinfo/file",
        json!({"workshopId": "../escape", "modinfo": "name=\"bad\""}),
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!dir.path().join("escape").exists());
}

#[tokio::test]
async fn manual_modinfo_file_rejects_oversized_lua_before_writing() {
    let (app, dir, _pool, _http) = test_router(Vec::new()).await;
    let cookie = login(&app).await;
    let oversized = "a".repeat(1024 * 1024 + 1);

    let response = send_json(
        &app,
        Method::POST,
        "/api/mod/modinfo/file",
        json!({"workshopId": "too-large", "modinfo": oversized}),
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        !dir.path()
            .join("mod-download/steamapps/workshop/content/322330/too-large/modinfo.lua")
            .exists()
    );
}

#[tokio::test]
async fn manual_modinfo_parser_returns_on_malformed_table_input() {
    let (app, _dir, _pool, _http) = test_router(Vec::new()).await;
    let cookie = login(&app).await;

    let response = timeout(
        Duration::from_secs(1),
        send_json(
            &app,
            Method::POST,
            "/api/mod/modinfo/file",
            json!({"workshopId": "malformed-mod", "modinfo": "name = \"Broken\"\nconfiguration_options = { = }\n"}),
            Some(&cookie),
        ),
    )
    .await
    .expect("malformed modinfo parser should not hang");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn manual_modinfo_parser_caps_deeply_nested_tables() {
    let (app, _dir, _pool, _http) = test_router(Vec::new()).await;
    let cookie = login(&app).await;
    let deep_table = format!("{}\"leaf\"{}", "{".repeat(80), "}".repeat(80));

    let response = timeout(
        Duration::from_secs(1),
        send_json(
            &app,
            Method::POST,
            "/api/mod/modinfo/file",
            json!({"workshopId": "deep-mod", "modinfo": format!("name = \"Deep\"\ndeep = {deep_table}\n")}),
            Some(&cookie),
        ),
    )
    .await
    .expect("deep modinfo parser should not overflow or hang");
    assert_eq!(response.status(), StatusCode::OK);

    let raw = send(
        &app,
        Method::GET,
        "/api/mod/modinfo/deep-mod",
        None,
        Some(&cookie),
    )
    .await;
    let raw_body = response_json(raw).await;
    let raw_config: Value =
        serde_json::from_str(raw_body["data"]["mod_config"].as_str().unwrap()).unwrap();
    assert!(raw_config.get("deep").is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn delete_mod_rejects_symlinked_download_content_root_without_soft_delete() {
    let (app, dir, _pool, _http) = test_router(Vec::new()).await;
    let cookie = login(&app).await;
    let save = send_json(
        &app,
        Method::POST,
        "/api/mod/modinfo",
        json!({"modid": "localmod", "name": "Local Mod", "mod_config": "{}"}),
        Some(&cookie),
    )
    .await;
    assert_eq!(save.status(), StatusCode::OK);

    let content_parent = dir.path().join("mod-download/steamapps/workshop/content");
    fs::create_dir_all(&content_parent).unwrap();
    let escape = dir.path().join("escape-download-root");
    fs::create_dir_all(escape.join("localmod")).unwrap();
    fs::write(escape.join("localmod/modinfo.lua"), "").unwrap();
    symlink(&escape, content_parent.join("322330")).unwrap();

    let response = send(
        &app,
        Method::DELETE,
        "/api/mod/localmod",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(escape.join("localmod/modinfo.lua").is_file());
    let list = send(&app, Method::GET, "/api/mod", None, Some(&cookie)).await;
    assert_eq!(response_json(list).await["data"][0]["modid"], "localmod");
}

#[tokio::test]
async fn mod_search_numeric_id_uses_steam_details_shape() {
    let (app, _dir, _pool, http) =
        test_router(vec![steam_details_response("123456", "Search Mod")]).await;
    let cookie = login(&app).await;

    let response = send(
        &app,
        Method::GET,
        "/api/mod/search?text=123456&page=3&size=20&lang=zh",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["code"], 200);
    assert_eq!(body["data"]["page"], 1);
    assert_eq!(body["data"]["size"], 1);
    assert_eq!(body["data"]["total"], 1);
    assert_eq!(body["data"]["data"][0]["id"], "123456");
    assert_eq!(body["data"]["data"][0]["name"], "Search Mod");
    assert_eq!(
        body["data"]["data"][0]["vote"],
        json!({"star": 0, "num": 0})
    );
    assert_eq!(
        body["data"]["data"][0]["author"],
        "https://steamcommunity.com/profiles/7656119/?xml=1"
    );

    let calls = http.requests();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0]
            .url
            .starts_with("http://api.steampowered.com/IPublishedFileService/GetDetails/v1/?")
    );
    assert!(!calls[0].url.contains("73DF9F"));
}

#[tokio::test]
async fn mod_search_numeric_uses_zh_steam_detail_language_for_go_parity() {
    let (app, _dir, _pool, http) =
        test_router(vec![steam_details_response("123456", "Search Mod")]).await;
    let cookie = login(&app).await;

    let response = send(
        &app,
        Method::GET,
        "/api/mod/search?text=123456&lang=en",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let calls = http.requests();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].url.contains("language=6"));
}

#[tokio::test]
async fn mod_search_numeric_accepts_real_steam_views_integer() {
    let (app, _dir, _pool, _http) = test_router(vec![steam_details_response_with_views_integer(
        "123456",
        "Views Int Mod",
    )])
    .await;
    let cookie = login(&app).await;

    let response = send(
        &app,
        Method::GET,
        "/api/mod/search?text=123456",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["data"]["data"][0]["id"], "123456");
    assert_eq!(body["data"]["data"][0]["name"], "Views Int Mod");
}

#[tokio::test]
async fn mod_search_numeric_id_without_steam_api_key_returns_manual_candidate() {
    let (app, _dir, _pool, http) = test_router_without_steam_api_key(Vec::new()).await;
    let cookie = login(&app).await;

    let response = send(
        &app,
        Method::GET,
        "/api/mod/search?text=123456",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["data"]["data"][0]["id"], "123456");
    assert_eq!(body["data"]["data"][0]["modid"], "123456");
    assert_eq!(body["data"]["data"][0]["name"], "workshop-123456");
    assert!(http.requests().is_empty());
}

#[tokio::test]
async fn mod_subscribe_numeric_id_without_steam_api_key_downloads_modinfo_with_steamcmd() {
    let command_runner = WritingCommandRunner::new("123456", modinfo_lua("No Key Mod"));
    let calls = command_runner.clone();
    let (app, _dir, _pool, _http) =
        test_router_without_steam_api_key_with_command_runner(Vec::new(), command_runner).await;
    let cookie = login(&app).await;

    let response = send(
        &app,
        Method::GET,
        "/api/mod/123456?lang=zh",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["data"]["modid"], "123456");
    assert_eq!(body["data"]["name"], "123456");
    assert_eq!(
        body["data"]["mod_config"]["configuration_options"][0]["name"],
        "difficulty"
    );
    assert_eq!(calls.calls().len(), 1);
}

#[tokio::test]
async fn mod_subscribe_without_steam_api_key_saves_empty_config_when_steamcmd_fails() {
    let command_runner = FakeCommandRunner::new(vec![failed_command_output()]);
    let calls = command_runner.clone();
    let (app, _dir, _pool, _http) =
        test_router_without_steam_api_key_with_command_runner(Vec::new(), command_runner).await;
    let cookie = login(&app).await;

    let response = send(
        &app,
        Method::GET,
        "/api/mod/123456?lang=zh",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["data"]["modid"], "123456");
    assert_eq!(body["data"]["name"], "123456");
    assert_eq!(body["data"]["mod_config"], json!({}));
    assert_eq!(calls.calls().len(), 1);
}

#[tokio::test]
async fn local_mod_get_creates_legacy_record_without_network() {
    let (app, _dir, _pool, http) = test_router(Vec::new()).await;
    let cookie = login(&app).await;

    let response = send(
        &app,
        Method::GET,
        "/api/mod/local-safe",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["code"], 200);
    assert_eq!(body["data"]["modid"], "local-safe");
    assert_eq!(body["data"]["name"], "local-safe");
    assert_eq!(body["data"]["img"], "xxx");
    assert_eq!(body["data"]["mod_config"], json!({}));
    assert!(http.requests().is_empty());
}

#[tokio::test]
async fn steam_mod_get_reads_existing_downloaded_modinfo_lua() {
    let (app, dir, _pool, _http) =
        test_router(vec![steam_details_response("123456", "Steam Mod")]).await;
    let cookie = login(&app).await;
    let mod_dir = dir
        .path()
        .join("mod-download/steamapps/workshop/content/322330/123456");
    fs::create_dir_all(&mod_dir).unwrap();
    fs::write(mod_dir.join("modinfo.lua"), modinfo_lua("Steam Mod")).unwrap();

    let response = send(&app, Method::GET, "/api/mod/123456", None, Some(&cookie)).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["data"]["name"], "Steam Mod");
    assert_eq!(body["data"]["mod_config"]["name"], "Steam Mod");
    assert_eq!(
        body["data"]["mod_config"]["configuration_options"][0]["name"],
        "difficulty"
    );
}

#[tokio::test]
async fn steam_mod_get_uses_file_url_zip_modinfo_without_steamcmd() {
    let command_runner = FakeCommandRunner::new(Vec::new());
    let zip = stored_zip(&[("modinfo.lua", b"name = \"Zip Mod\"")]);
    let (app, _dir, _pool, http) = test_router_with_command_runner(
        vec![
            steam_details_response_with_file_url(
                "123456",
                "Zip Detail Mod",
                "https://steamusercontent-a.akamaihd.net/mod.zip",
            ),
            HttpResponse::new(200)
                .header("content-type", "application/zip")
                .body(zip),
        ],
        command_runner.clone(),
    )
    .await;
    let cookie = login(&app).await;

    let response = send(
        &app,
        Method::GET,
        "/api/mod/123456?lang=en",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(
        body["data"]["file_url"],
        "https://steamusercontent-a.akamaihd.net/mod.zip"
    );
    assert_eq!(body["data"]["mod_config"]["name"], "Zip Mod");
    assert!(command_runner.calls().is_empty());
    let calls = http.requests();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].url.contains("language=6"));
    assert_eq!(
        calls[1].url,
        "https://steamusercontent-a.akamaihd.net/mod.zip"
    );
}

#[tokio::test]
async fn steam_mod_get_trusts_current_steamusercontent_cdn_file_url() {
    let command_runner = FakeCommandRunner::new(Vec::new());
    let zip = stored_zip(&[("modinfo.lua", b"name = \"Steam CDN Zip Mod\"")]);
    let (app, _dir, _pool, http) = test_router_with_command_runner(
        vec![
            steam_details_response_with_file_url(
                "123456",
                "Steam CDN Detail Mod",
                "https://cdn.steamusercontent.com/mod.zip",
            ),
            HttpResponse::new(200)
                .header("content-type", "application/zip")
                .body(zip),
        ],
        command_runner.clone(),
    )
    .await;
    let cookie = login(&app).await;

    let response = send(&app, Method::GET, "/api/mod/123456", None, Some(&cookie)).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["data"]["mod_config"]["name"], "Steam CDN Zip Mod");
    assert!(command_runner.calls().is_empty());
    let calls = http.requests();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].url, "https://cdn.steamusercontent.com/mod.zip");
}

#[tokio::test]
async fn steam_mod_get_falls_back_to_steamcmd_for_untrusted_file_url() {
    let command_runner = WritingCommandRunner::new("123456", modinfo_lua("Fallback SteamCMD Mod"));
    let (app, _dir, _pool, http) = test_router_with_command_runner(
        vec![steam_details_response_with_file_url(
            "123456",
            "Untrusted File URL Mod",
            "https://evil.example.test/mod.zip",
        )],
        command_runner.clone(),
    )
    .await;
    let cookie = login(&app).await;

    let response = send(&app, Method::GET, "/api/mod/123456", None, Some(&cookie)).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["data"]["mod_config"]["name"], "Fallback SteamCMD Mod");
    assert_eq!(command_runner.calls().len(), 1);
    let calls = http.requests();
    assert_eq!(calls.len(), 1);
    assert_ne!(calls[0].url, "https://evil.example.test/mod.zip");
}

#[tokio::test]
async fn steam_mod_get_saves_metadata_with_empty_config_when_steamcmd_fails() {
    let command_runner = FakeCommandRunner::new(vec![failed_command_output()]);
    let (app, _dir, _pool, _http) = test_router_with_command_runner(
        vec![steam_details_response("123456", "SteamCMD Failure Mod")],
        command_runner.clone(),
    )
    .await;
    let cookie = login(&app).await;

    let response = send(&app, Method::GET, "/api/mod/123456", None, Some(&cookie)).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["data"]["modid"], "123456");
    assert_eq!(body["data"]["name"], "SteamCMD Failure Mod");
    assert_eq!(body["data"]["mod_config"], json!({}));
    assert_eq!(command_runner.calls().len(), 1);
}

#[tokio::test]
async fn mod_put_redownloads_and_reparses_modinfo_after_cache_delete() {
    let command_runner = WritingCommandRunner::new("123456", modinfo_lua("Redownloaded Mod"));
    let (app, dir, _pool, _http) = test_router_with_command_runner(
        vec![
            steam_details_response("123456", "Seed Mod"),
            steam_details_response("123456", "Redownloaded Mod"),
        ],
        command_runner.clone(),
    )
    .await;
    let cookie = login(&app).await;

    let old_dir = dir
        .path()
        .join("mod-download/steamapps/workshop/content/322330/123456");
    fs::create_dir_all(&old_dir).unwrap();
    fs::write(old_dir.join("modinfo.lua"), modinfo_lua("Seed Mod")).unwrap();
    let steamcmd64 = dir.path().join(if cfg!(windows) {
        "steamcmd/steamcmd.exe"
    } else {
        "steamcmd/linux64/steamcmd"
    });
    fs::create_dir_all(steamcmd64.parent().unwrap()).unwrap();
    fs::write(&steamcmd64, "").unwrap();

    let seed = send(&app, Method::GET, "/api/mod/123456", None, Some(&cookie)).await;
    assert_eq!(seed.status(), StatusCode::OK);

    let response = send(&app, Method::PUT, "/api/mod/123456", None, Some(&cookie)).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["data"]["modid"], "123456");
    assert_eq!(body["data"]["name"], "Redownloaded Mod");
    assert_eq!(body["data"]["mod_config"]["name"], "Redownloaded Mod");

    let calls = command_runner.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].program().ends_with(if cfg!(windows) {
        "steamcmd\\steamcmd.exe"
    } else {
        "steamcmd/linux64/steamcmd"
    }));
    assert!(
        calls[0]
            .args()
            .contains(&"+workshop_download_item".to_owned())
    );
    assert!(calls[0].args().contains(&"322330".to_owned()));
    assert!(calls[0].args().contains(&"123456".to_owned()));
}

#[tokio::test]
async fn mod_put_preserves_existing_row_and_cache_when_redownload_fails() {
    let command_runner = FakeCommandRunner::new(vec![failed_command_output()]);
    let (app, dir, _pool, _http) = test_router_with_command_runner(
        vec![steam_details_response("123456", "Refresh Failure Mod")],
        command_runner.clone(),
    )
    .await;
    let cookie = login(&app).await;

    let seed = send_json(
        &app,
        Method::POST,
        "/api/mod/modinfo",
        json!({
            "modid": "123456",
            "name": "Existing Mod",
            "last_time": 1.0,
            "mod_config": "{\"name\":\"Existing Mod\"}"
        }),
        Some(&cookie),
    )
    .await;
    assert_eq!(seed.status(), StatusCode::OK);
    let mod_dir = dir
        .path()
        .join("mod-download/steamapps/workshop/content/322330/123456");
    fs::create_dir_all(&mod_dir).unwrap();
    fs::write(mod_dir.join("modinfo.lua"), "name = \"Existing Mod\"").unwrap();

    let response = send(&app, Method::PUT, "/api/mod/123456", None, Some(&cookie)).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(command_runner.calls().len(), 1);
    assert_eq!(
        fs::read_to_string(mod_dir.join("modinfo.lua")).unwrap(),
        "name = \"Existing Mod\""
    );
    let raw = send(
        &app,
        Method::GET,
        "/api/mod/modinfo/123456",
        None,
        Some(&cookie),
    )
    .await;
    let raw_body = response_json(raw).await;
    assert_eq!(raw_body["data"]["modid"], "123456");
    assert_eq!(raw_body["data"]["name"], "Existing Mod");
}

#[tokio::test]
async fn update_all_modinfo_redownloads_changed_mod_and_replaces_config() {
    let command_runner = WritingCommandRunner::new("123456", modinfo_lua("Batch Updated Mod"));
    let (app, _dir, _pool, _http) = test_router_with_command_runner(
        vec![steam_details_response("123456", "Batch Updated Mod")],
        command_runner.clone(),
    )
    .await;
    let cookie = login(&app).await;

    let seed = send_json(
        &app,
        Method::POST,
        "/api/mod/modinfo",
        json!({
            "modid": "123456",
            "name": "Old Mod",
            "last_time": 1.0,
            "mod_config": "{\"name\":\"Old Mod\"}"
        }),
        Some(&cookie),
    )
    .await;
    assert_eq!(seed.status(), StatusCode::OK);

    let response = send(
        &app,
        Method::PUT,
        "/api/mod/modinfo?lang=zh",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(command_runner.calls().len(), 1);

    let raw = send(
        &app,
        Method::GET,
        "/api/mod/modinfo/123456",
        None,
        Some(&cookie),
    )
    .await;
    let raw_body = response_json(raw).await;
    assert_eq!(raw_body["data"]["last_time"], 1710001234.0);
    let raw_config: Value =
        serde_json::from_str(raw_body["data"]["mod_config"].as_str().unwrap()).unwrap();
    assert_eq!(raw_config["name"], "Batch Updated Mod");
    assert_eq!(raw_body["data"]["name"], "Batch Updated Mod");
}

#[tokio::test]
async fn update_all_modinfo_preserves_existing_cache_when_redownload_fails() {
    let command_runner = FakeCommandRunner::new(vec![failed_command_output()]);
    let (app, dir, _pool, _http) = test_router_with_command_runner(
        vec![steam_details_response("123456", "Batch Failure Mod")],
        command_runner.clone(),
    )
    .await;
    let cookie = login(&app).await;

    let seed = send_json(
        &app,
        Method::POST,
        "/api/mod/modinfo",
        json!({
            "modid": "123456",
            "name": "Old Batch Mod",
            "last_time": 1.0,
            "mod_config": "{\"name\":\"Old Batch Mod\"}"
        }),
        Some(&cookie),
    )
    .await;
    assert_eq!(seed.status(), StatusCode::OK);
    let mod_dir = dir
        .path()
        .join("mod-download/steamapps/workshop/content/322330/123456");
    fs::create_dir_all(&mod_dir).unwrap();
    fs::write(mod_dir.join("modinfo.lua"), "name = \"Old Batch Mod\"").unwrap();

    let response = send(
        &app,
        Method::PUT,
        "/api/mod/modinfo?lang=zh",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(command_runner.calls().len(), 1);
    assert_eq!(
        fs::read_to_string(mod_dir.join("modinfo.lua")).unwrap(),
        "name = \"Old Batch Mod\""
    );
    let raw = send(
        &app,
        Method::GET,
        "/api/mod/modinfo/123456",
        None,
        Some(&cookie),
    )
    .await;
    let raw_body = response_json(raw).await;
    assert_eq!(raw_body["data"]["name"], "Old Batch Mod");
}

#[tokio::test]
async fn mod_put_rolls_back_when_steamcmd_succeeds_without_modinfo_lua() {
    let command_runner = FakeCommandRunner::new(vec![CommandOutput::success(
        b"downloaded".to_vec(),
        Vec::new(),
    )]);
    let (app, dir, _pool, _http) = test_router_with_command_runner(
        vec![steam_details_response("123456", "Missing Modinfo Mod")],
        command_runner.clone(),
    )
    .await;
    let cookie = login(&app).await;

    let seed = send_json(
        &app,
        Method::POST,
        "/api/mod/modinfo",
        json!({
            "modid": "123456",
            "name": "Existing Missing",
            "last_time": 1.0,
            "mod_config": "{\"name\":\"Existing Missing\"}"
        }),
        Some(&cookie),
    )
    .await;
    assert_eq!(seed.status(), StatusCode::OK);
    let mod_dir = dir
        .path()
        .join("mod-download/steamapps/workshop/content/322330/123456");
    fs::create_dir_all(&mod_dir).unwrap();
    fs::write(mod_dir.join("modinfo.lua"), "name = \"Existing Missing\"").unwrap();

    let response = send(&app, Method::PUT, "/api/mod/123456", None, Some(&cookie)).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(command_runner.calls().len(), 1);
    assert_eq!(
        fs::read_to_string(mod_dir.join("modinfo.lua")).unwrap(),
        "name = \"Existing Missing\""
    );
    let raw = send(
        &app,
        Method::GET,
        "/api/mod/modinfo/123456",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(response_json(raw).await["data"]["name"], "Existing Missing");
}

#[tokio::test]
async fn mod_search_text_uses_query_files_go_shape() {
    let (app, _dir, _pool, _http) = test_router(vec![steam_query_response()]).await;
    let cookie = login(&app).await;

    let response = send(
        &app,
        Method::GET,
        "/api/mod/search?text=forest&page=2&size=5&lang=en",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["data"]["page"], 2);
    assert_eq!(body["data"]["size"], 5);
    assert_eq!(body["data"]["total"], 11);
    assert_eq!(body["data"]["totalPage"], 3);
    assert_eq!(body["data"]["data"][0]["id"], "654321");
    assert_eq!(
        body["data"]["data"][0]["img"],
        "https://cdn.example.test/query.jpg"
    );
    assert_eq!(body["data"]["data"][0]["file_url"], "");
    assert_eq!(body["data"]["data"][0]["v"], "");
    assert_eq!(body["data"]["data"][0]["last_time"], 0.0);
    assert_eq!(body["data"]["data"][0]["consumer_appid"], 0.0);
    assert_eq!(body["data"]["data"][0]["creator_appid"], 0.0);
    assert_eq!(
        body["data"]["data"][0]["vote"],
        json!({"star": 4, "num": 7})
    );
}

#[cfg(unix)]
#[tokio::test]
async fn setup_workshop_delete_rejects_symlinked_mods_root_without_touching_target() {
    let (app, dir, _pool, _http) = test_router(Vec::new()).await;
    let cookie = login(&app).await;
    let force_install_dir = dir.path().join("dst-dedicated-server");
    fs::create_dir_all(&force_install_dir).unwrap();
    let escape = dir.path().join("escape-mods-root");
    fs::create_dir_all(escape.join("workshop-123")).unwrap();
    symlink(&escape, force_install_dir.join("mods")).unwrap();

    let response = send(
        &app,
        Method::DELETE,
        "/api/mod/setup/workshop",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(escape.join("workshop-123").is_dir());
}

#[tokio::test]
async fn setup_workshop_delete_removes_only_workshop_named_directories() {
    let (app, dir, _pool, _http) = test_router(Vec::new()).await;
    let cookie = login(&app).await;
    let mods_dir = dir.path().join("dst-dedicated-server/mods");
    fs::create_dir_all(mods_dir.join("workshop-123")).unwrap();
    fs::create_dir_all(mods_dir.join("server-only")).unwrap();
    fs::write(mods_dir.join("server-only/modmain.lua"), "").unwrap();

    let response = send(
        &app,
        Method::DELETE,
        "/api/mod/setup/workshop",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_json(response).await,
        json!({"code": 200, "msg": "success", "data": null})
    );
    assert!(!mods_dir.join("workshop-123").exists());
    assert!(mods_dir.join("server-only").is_dir());
}

#[cfg(unix)]
#[tokio::test]
async fn ugc_acf_rejects_symlinked_acf_file_without_calling_steam() {
    let (app, dir, _pool, http) = test_router(Vec::new()).await;
    let cookie = login(&app).await;
    let acf_path = dir
        .path()
        .join("dst-dedicated-server/ugc_mods/ClusterA/Master/appworkshop_322330.acf");
    fs::create_dir_all(acf_path.parent().unwrap()).unwrap();
    let escape = dir.path().join("escape-acf");
    fs::write(&escape, "\"WorkshopItemsInstalled\"").unwrap();
    symlink(&escape, &acf_path).unwrap();

    let response = send(
        &app,
        Method::GET,
        "/api/mod/ugc/acf?levelName=Master",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(http.requests().is_empty());
}

#[tokio::test]
async fn ugc_upload_writes_uploaded_file_under_master_content_root() {
    let (app, dir, _pool, _http) = test_router(Vec::new()).await;
    let cookie = login(&app).await;
    let (body, content_type) = multipart_body(vec![
        text_part("filePaths", "123456/modmain.lua"),
        file_part("files", "modmain.lua", "text/plain", b"name = \"UGC\""),
    ]);

    let response = send_raw(
        &app,
        Method::POST,
        "/api/file/ugc/upload",
        body,
        Some(&content_type),
        Some(&cookie),
        Vec::new(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_json(response).await,
        json!({"code": 200, "msg": "success", "data": null})
    );
    let written = dir
        .path()
        .join("dst-dedicated-server/ugc_mods/ClusterA/Master/content/322330/123456/modmain.lua");
    assert_eq!(fs::read_to_string(written).unwrap(), "name = \"UGC\"");
}

#[cfg(unix)]
#[tokio::test]
async fn ugc_delete_rejects_symlinked_content_root_without_touching_target() {
    let (app, dir, _pool, _http) = test_router(Vec::new()).await;
    let cookie = login(&app).await;
    let content_parent = dir
        .path()
        .join("dst-dedicated-server/ugc_mods/ClusterA/Master/content");
    fs::create_dir_all(&content_parent).unwrap();
    let escape = dir.path().join("escape-ugc-content");
    fs::create_dir_all(escape.join("1185229307")).unwrap();
    fs::write(escape.join("1185229307/modmain.lua"), "").unwrap();
    symlink(&escape, content_parent.join("322330")).unwrap();

    let response = send(
        &app,
        Method::DELETE,
        "/api/mod/ugc?levelName=Master&workshopId=1185229307",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(escape.join("1185229307/modmain.lua").is_file());
}

#[tokio::test]
async fn ugc_upload_rejects_traversal_file_paths() {
    let (app, dir, _pool, _http) = test_router(Vec::new()).await;
    let cookie = login(&app).await;
    let (body, content_type) = multipart_body(vec![
        text_part("filePaths", "../escape/modmain.lua"),
        file_part("files", "modmain.lua", "text/plain", b"name = \"bad\""),
    ]);

    let response = send_raw(
        &app,
        Method::POST,
        "/api/file/ugc/upload",
        body,
        Some(&content_type),
        Some(&cookie),
        Vec::new(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        !dir.path()
            .join("dst-dedicated-server/ugc_mods/escape")
            .exists()
    );
}

#[tokio::test]
async fn background_upload_and_get_round_trip_png_from_root_path() {
    let (app, _dir, _pool, _http) = test_router(Vec::new()).await;
    let cookie = login(&app).await;
    let png = b"\x89PNG\r\n\x1a\npanel-background";
    let (body, content_type) =
        multipart_body(vec![file_part("file", "background.png", "image/png", png)]);

    let upload = send_raw(
        &app,
        Method::POST,
        "/api/file/background",
        body,
        Some(&content_type),
        Some(&cookie),
        Vec::new(),
    )
    .await;
    assert_eq!(upload.status(), StatusCode::OK);

    let get = send(
        &app,
        Method::GET,
        "/api/file/background",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(get.status(), StatusCode::OK);
    assert_eq!(get.headers()[CONTENT_TYPE], "image/png");
    assert_eq!(response_bytes(get).await, png);
}

#[tokio::test]
async fn background_upload_accepts_multiple_non_png_files_and_last_wins() {
    let (app, _dir, _pool, _http) = test_router(Vec::new()).await;
    let cookie = login(&app).await;
    let (body, content_type) = multipart_body(vec![
        file_part("file", "background.jpg", "image/jpeg", b"first-image"),
        file_part("file", "background.webp", "image/webp", b"second-image"),
    ]);

    let upload = send_raw(
        &app,
        Method::POST,
        "/api/file/background",
        body,
        Some(&content_type),
        Some(&cookie),
        Vec::new(),
    )
    .await;
    assert_eq!(upload.status(), StatusCode::OK);

    let get = send(
        &app,
        Method::GET,
        "/api/file/background",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(get.status(), StatusCode::OK);
    assert_eq!(response_bytes(get).await, b"second-image");
}

#[tokio::test]
async fn ugc_acf_reads_local_acf_and_enriches_steam_details() {
    let (app, dir, _pool, _http) =
        test_router(vec![steam_details_response("1185229307", "ACF Mod")]).await;
    let cookie = login(&app).await;
    let acf_path = dir
        .path()
        .join("dst-dedicated-server/ugc_mods/ClusterA/Master/appworkshop_322330.acf");
    fs::create_dir_all(acf_path.parent().unwrap()).unwrap();
    fs::write(
        &acf_path,
        r#""AppWorkshop"
{
    "WorkshopItemsInstalled"
    {
        "1185229307"
        {
            "timeupdated" "1710000000"
            "manifest" "manifest-id"
        }
    }
}"#,
    )
    .unwrap();

    let response = send(
        &app,
        Method::GET,
        "/api/mod/ugc/acf?levelName=Master",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["code"], 200);
    assert_eq!(body["data"][0]["workshopId"], "1185229307");
    assert_eq!(body["data"][0]["name"], "ACF Mod");
    assert_eq!(body["data"][0]["timeupdated"], 1710000000);
    assert_eq!(body["data"][0]["timelast"], 1710001234.0);
}

#[tokio::test]
async fn ugc_delete_removes_one_workshop_directory() {
    let (app, dir, _pool, _http) = test_router(Vec::new()).await;
    let cookie = login(&app).await;
    let target = dir
        .path()
        .join("dst-dedicated-server/ugc_mods/ClusterA/Master/content/322330/1185229307");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("modmain.lua"), "").unwrap();

    let response = send(
        &app,
        Method::DELETE,
        "/api/mod/ugc?levelName=Master&workshopId=1185229307",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(!target.exists());
}

#[tokio::test]
async fn dst_static_forwards_raw_get_without_sensitive_headers() {
    let (app, _dir, _pool, http) = test_router(vec![
        HttpResponse::new(200)
            .header("content-type", "text/plain")
            .body("asset bytes"),
    ])
    .await;
    let cookie = login(&app).await;

    let response = send_raw(
        &app,
        Method::GET,
        "/api/dst-static/images/foo%20bar.png",
        Vec::new(),
        None,
        Some(&cookie),
        vec![(AUTHORIZATION.as_str(), "Bearer should-not-forward")],
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
    assert_eq!(response_text(response).await, "asset bytes");

    let calls = http.requests();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].method, "GET");
    assert_eq!(
        calls[0].url,
        "https://gitee.com/hhhuhu23/dst-static/raw/master/images/foo%20bar.png"
    );
    assert!(
        calls[0]
            .headers
            .iter()
            .all(|(name, _)| !name.eq_ignore_ascii_case("cookie")
                && !name.eq_ignore_ascii_case("authorization"))
    );
}

#[tokio::test]
async fn dst_static_forwards_post_body_for_legacy_any_route() {
    let (app, _dir, _pool, http) = test_router(vec![HttpResponse::new(201).body("created")]).await;
    let cookie = login(&app).await;

    let response = send_raw(
        &app,
        Method::POST,
        "/api/dst-static/scripts/setup.lua",
        b"payload".to_vec(),
        Some("text/plain"),
        Some(&cookie),
        Vec::new(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response_text(response).await, "created");
    let calls = http.requests();
    assert_eq!(calls[0].method, "POST");
    assert_eq!(calls[0].body, b"payload");
}

#[tokio::test]
async fn dst_static_strips_cookie_and_hop_by_hop_response_headers() {
    let (app, _dir, _pool, _http) = test_router(vec![
        HttpResponse::new(200)
            .header("content-type", "text/plain")
            .header("set-cookie", "token=evil")
            .header("connection", "close")
            .header("transfer-encoding", "chunked")
            .header("x-static-version", "1")
            .body("asset bytes"),
    ])
    .await;
    let cookie = login(&app).await;

    let response = send(
        &app,
        Method::GET,
        "/api/dst-static/images/file.txt",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("set-cookie").is_none());
    assert!(response.headers().get("connection").is_none());
    assert!(response.headers().get("transfer-encoding").is_none());
    assert_eq!(response.headers()["x-static-version"], "1");
}

#[tokio::test]
async fn raw_modinfo_post_upserts_by_modid_when_id_is_omitted() {
    let (app, _dir, _pool, _http) = test_router(Vec::new()).await;
    let cookie = login(&app).await;

    let first = send_json(
        &app,
        Method::POST,
        "/api/mod/modinfo",
        json!({"modid": "dup-raw", "name": "First Raw", "mod_config": "{\"name\":\"First Raw\"}"}),
        Some(&cookie),
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let second = send_json(
        &app,
        Method::POST,
        "/api/mod/modinfo",
        json!({"modid": "dup-raw", "name": "Second Raw", "mod_config": "{\"name\":\"Second Raw\"}"}),
        Some(&cookie),
    )
    .await;
    assert_eq!(second.status(), StatusCode::OK);

    let list = send(&app, Method::GET, "/api/mod", None, Some(&cookie)).await;
    let body = response_json(list).await;
    let rows = body["data"].as_array().unwrap();
    assert_eq!(
        rows.iter().filter(|row| row["modid"] == "dup-raw").count(),
        1
    );
    assert_eq!(rows[0]["name"], "Second Raw");
    assert_eq!(rows[0]["mod_config"]["name"], "Second Raw");
}

#[tokio::test]
async fn dst_static_rejects_encoded_traversal_before_upstream_request() {
    let (app, _dir, _pool, http) = test_router(Vec::new()).await;
    let cookie = login(&app).await;

    let response = send(
        &app,
        Method::GET,
        "/api/dst-static/%2e%2e/secret.png",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(http.requests().is_empty());
}

async fn test_router(
    http_responses: Vec<HttpResponse>,
) -> (Router, TempDir, SqlitePool, FakeHttpClient) {
    test_router_with_command_runner(http_responses, FakeCommandRunner::new(Vec::new())).await
}

async fn test_router_with_command_runner<R>(
    http_responses: Vec<HttpResponse>,
    command_runner: R,
) -> (Router, TempDir, SqlitePool, FakeHttpClient)
where
    R: CommandRunner + 'static,
{
    test_router_with_config_and_command_runner(test_config(), http_responses, command_runner).await
}

async fn test_router_without_steam_api_key(
    http_responses: Vec<HttpResponse>,
) -> (Router, TempDir, SqlitePool, FakeHttpClient) {
    test_router_without_steam_api_key_with_command_runner(
        http_responses,
        FakeCommandRunner::new(Vec::new()),
    )
    .await
}

async fn test_router_without_steam_api_key_with_command_runner<R>(
    http_responses: Vec<HttpResponse>,
    command_runner: R,
) -> (Router, TempDir, SqlitePool, FakeHttpClient)
where
    R: CommandRunner + 'static,
{
    test_router_with_config_and_command_runner(
        test_config_without_steam_api_key(),
        http_responses,
        command_runner,
    )
    .await
}

async fn test_router_with_config_and_command_runner<R>(
    config: AppConfig,
    http_responses: Vec<HttpResponse>,
    command_runner: R,
) -> (Router, TempDir, SqlitePool, FakeHttpClient)
where
    R: CommandRunner + 'static,
{
    let dir = tempdir().unwrap();
    write_password_file(dir.path());
    write_dst_config(dir.path());
    let pool = connect_sqlite_memory().await.unwrap();
    migrate(&pool).await.unwrap();
    let http = FakeHttpClient::new(http_responses);
    let state = AppState::new_with_command_runner_and_http_client(
        config,
        pool.clone(),
        SessionStore::new(),
        dir.path(),
        command_runner,
        http.clone(),
        SystemProcessSnapshotProvider,
    );
    (build_router(state), dir, pool, http)
}

#[derive(Clone)]
struct WritingCommandRunner {
    mod_id: String,
    lua: String,
    calls: Arc<Mutex<Vec<CommandSpec>>>,
}

impl WritingCommandRunner {
    fn new(mod_id: &str, lua: String) -> Self {
        Self {
            mod_id: mod_id.to_owned(),
            lua,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn calls(&self) -> Vec<CommandSpec> {
        self.calls.lock().unwrap().clone()
    }
}

impl CommandRunner for WritingCommandRunner {
    fn run<'a>(&'a self, spec: CommandSpec) -> CommandFuture<'a> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(spec.clone());
            let force_install_dir = spec
                .args()
                .windows(2)
                .find_map(|args| (args[0] == "+force_install_dir").then(|| args[1].clone()))
                .expect("SteamCMD command must pass +force_install_dir as argv");
            let target_dir = Path::new(&force_install_dir)
                .join("steamapps")
                .join("workshop")
                .join("content")
                .join("322330")
                .join(&self.mod_id);
            fs::create_dir_all(&target_dir).unwrap();
            fs::write(target_dir.join("modinfo.lua"), &self.lua).unwrap();
            Ok(CommandOutput::success(
                b"Downloaded item\n".to_vec(),
                Vec::new(),
            ))
        })
    }
}

fn test_config() -> AppConfig {
    AppConfig {
        bind_address: "127.0.0.1".to_owned(),
        port: "0".to_owned(),
        path: String::new(),
        data_dir: String::new(),
        database: ":memory:".to_owned(),
        steamcmd: String::new(),
        steam_api_key: Some("test-steam-key".to_owned()),
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

fn test_config_without_steam_api_key() -> AppConfig {
    AppConfig {
        steam_api_key: None,
        ..test_config()
    }
}

fn write_password_file(root_path: &Path) {
    fs::write(
        root_path.join("password.txt"),
        "username=admin\npassword=123456\ndisplayName=Admin\nphotoURL=https://example.test/avatar.png\n",
    )
    .unwrap();
}

fn write_dst_config(root_path: &Path) {
    fs::write(
        root_path.join("dst_config"),
        format!(
            "steamcmd={0}/steamcmd\nforce_install_dir={0}/dst-dedicated-server\ndonot_starve_server_directory=\nugc_directory=\nconf_dir=\npersistent_storage_root=\ncluster=ClusterA\nbackup={0}/backup\nmod_download_path={0}/mod-download\nbin=32\nbeta=0\n",
            root_path.display()
        ),
    )
    .unwrap();
}

fn steam_details_response(id: &str, title: &str) -> HttpResponse {
    HttpResponse::new(200)
        .header("content-type", "application/json")
        .body(
            json!({
                "response": {
                    "publishedfiledetails": [{
                        "publishedfileid": id,
                        "title": title,
                        "creator": "7656119",
                        "creator_appid": 322330.0,
                        "consumer_appid": 322330.0,
                        "file_description": "Steam description",
                        "preview_url": "https://cdn.example.test/preview.jpg",
                        "time_updated": 1710001234.0,
                        "subscriptions": 42.0,
                        "views": [{"tag": "version:2.3"}],
                        "file_url": ""
                    }]
                }
            })
            .to_string(),
        )
}

fn steam_details_response_with_views_integer(id: &str, title: &str) -> HttpResponse {
    HttpResponse::new(200)
        .header("content-type", "application/json")
        .body(
            json!({
                "response": {
                    "publishedfiledetails": [{
                        "publishedfileid": id,
                        "title": title,
                        "creator": "7656119",
                        "creator_appid": 322330.0,
                        "consumer_appid": 322330.0,
                        "file_description": "Steam description",
                        "preview_url": "https://cdn.example.test/preview.jpg",
                        "time_updated": 1710001234.0,
                        "subscriptions": 42.0,
                        "views": 123,
                        "file_url": ""
                    }]
                }
            })
            .to_string(),
        )
}

fn steam_details_response_with_file_url(id: &str, title: &str, file_url: &str) -> HttpResponse {
    HttpResponse::new(200)
        .header("content-type", "application/json")
        .body(
            json!({
                "response": {
                    "publishedfiledetails": [{
                        "publishedfileid": id,
                        "title": title,
                        "creator": "7656119",
                        "creator_appid": 322330.0,
                        "consumer_appid": 322330.0,
                        "file_description": "Steam description",
                        "preview_url": "https://cdn.example.test/preview.jpg",
                        "time_updated": 1710001234.0,
                        "subscriptions": 42.0,
                        "views": 123,
                        "file_url": file_url
                    }]
                }
            })
            .to_string(),
        )
}

fn steam_query_response() -> HttpResponse {
    HttpResponse::new(200)
        .header("content-type", "application/json")
        .body(
            json!({
                "response": {
                    "total": 11,
                    "publishedfiledetails": [{
                        "publishedfileid": "654321",
                        "title": "Query Mod",
                        "creator": "7656000",
                        "file_description": "Query description",
                        "preview_url": "https://cdn.example.test/query.jpg",
                        "time_updated": 1710001234.0,
                        "subscriptions": 9.0,
                        "vote_data": {
                            "score": 0.6,
                            "votes_up": 4.0,
                            "votes_down": 3.0
                        },
                        "num_children": 0.0,
                        "children": []
                    }]
                }
            })
            .to_string(),
        )
}

fn modinfo_lua(name: &str) -> String {
    format!(
        r#"name = "{name}"
api_version = 10
dst_compatible = true
all_clients_require_mod = false
configuration_options = {{
  {{
    name = "difficulty",
    label = "Difficulty",
    options = {{
      {{ description = "Easy", data = "easy" }},
      {{ description = "Hard", data = "hard" }}
    }},
    default = "easy"
  }}
}}
"#
    )
}

fn failed_command_output() -> CommandOutput {
    CommandOutput {
        status_code: Some(1),
        stdout: Vec::new(),
        stderr: b"failed".to_vec(),
        timed_out: false,
        stdout_truncated: false,
        stderr_truncated: false,
    }
}

fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut central_directory = Vec::new();

    for (name, contents) in entries {
        let offset = bytes.len() as u32;
        let crc = crc32(contents);
        write_u32(&mut bytes, 0x0403_4b50);
        write_u16(&mut bytes, 20);
        write_u16(&mut bytes, 0);
        write_u16(&mut bytes, 0);
        write_u16(&mut bytes, 0);
        write_u16(&mut bytes, 0);
        write_u32(&mut bytes, crc);
        write_u32(&mut bytes, contents.len() as u32);
        write_u32(&mut bytes, contents.len() as u32);
        write_u16(&mut bytes, name.len() as u16);
        write_u16(&mut bytes, 0);
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(contents);

        write_u32(&mut central_directory, 0x0201_4b50);
        write_u16(&mut central_directory, 20);
        write_u16(&mut central_directory, 20);
        write_u16(&mut central_directory, 0);
        write_u16(&mut central_directory, 0);
        write_u16(&mut central_directory, 0);
        write_u16(&mut central_directory, 0);
        write_u32(&mut central_directory, crc);
        write_u32(&mut central_directory, contents.len() as u32);
        write_u32(&mut central_directory, contents.len() as u32);
        write_u16(&mut central_directory, name.len() as u16);
        write_u16(&mut central_directory, 0);
        write_u16(&mut central_directory, 0);
        write_u16(&mut central_directory, 0);
        write_u16(&mut central_directory, 0);
        write_u32(&mut central_directory, 0);
        write_u32(&mut central_directory, offset);
        central_directory.extend_from_slice(name.as_bytes());
    }

    let central_offset = bytes.len() as u32;
    let central_size = central_directory.len() as u32;
    bytes.extend_from_slice(&central_directory);
    write_u32(&mut bytes, 0x0605_4b50);
    write_u16(&mut bytes, 0);
    write_u16(&mut bytes, 0);
    write_u16(&mut bytes, entries.len() as u16);
    write_u16(&mut bytes, entries.len() as u16);
    write_u32(&mut bytes, central_size);
    write_u32(&mut bytes, central_offset);
    write_u16(&mut bytes, 0);
    bytes
}

fn write_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn write_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

async fn login(app: &Router) -> String {
    let response = send_json(
        app,
        Method::POST,
        "/api/login",
        json!({"username": "admin", "password": "123456"}),
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

async fn send_json(
    app: &Router,
    method: Method,
    uri: &str,
    json_body: Value,
    cookie: Option<&str>,
) -> Response<Body> {
    send_raw(
        app,
        method,
        uri,
        json_body.to_string().into_bytes(),
        Some("application/json"),
        cookie,
        Vec::new(),
    )
    .await
}

async fn send(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<Vec<u8>>,
    cookie: Option<&str>,
) -> Response<Body> {
    send_raw(
        app,
        method,
        uri,
        body.unwrap_or_default(),
        None,
        cookie,
        Vec::new(),
    )
    .await
}

async fn send_raw(
    app: &Router,
    method: Method,
    uri: &str,
    body: Vec<u8>,
    content_type: Option<&str>,
    cookie: Option<&str>,
    headers: Vec<(&str, &str)>,
) -> Response<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(content_type) = content_type {
        builder = builder.header(CONTENT_TYPE, content_type);
    }
    if let Some(cookie) = cookie {
        builder = builder.header(COOKIE, cookie);
    }
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    app.clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap()
}

async fn response_json(response: Response<Body>) -> Value {
    let status = response.status();
    let bytes = response_bytes(response).await;
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("expected JSON response for status {status}: {error}"))
}

async fn response_text(response: Response<Body>) -> String {
    String::from_utf8(response_bytes(response).await).unwrap()
}

async fn response_bytes(response: Response<Body>) -> Vec<u8> {
    to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

fn multipart_body(parts: Vec<Vec<u8>>) -> (Vec<u8>, String) {
    let boundary = "slice2-test-boundary";
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(&part);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (body, format!("multipart/form-data; boundary={boundary}"))
}

fn text_part(name: &str, value: &str) -> Vec<u8> {
    format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}").into_bytes()
}

fn file_part(name: &str, filename: &str, content_type: &str, contents: &[u8]) -> Vec<u8> {
    let mut part = format!(
        "Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
    )
    .into_bytes();
    part.extend_from_slice(contents);
    part
}
