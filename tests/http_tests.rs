use std::{fs, net::SocketAddr, path::Path};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{
        Method, Request, Response, StatusCode,
        header::{CACHE_CONTROL, CONTENT_TYPE, COOKIE, HeaderName, HeaderValue, SET_COOKIE},
    },
    response::IntoResponse,
};
use dst_admin_rust::{
    domain::auth::SessionStore,
    infra::config::{AppConfig, AutoUpdateModinfoConfig},
    infra::db::{connect_sqlite_memory, migrate},
    infra::logging,
    web::app::{AppState, build_connect_info_service, build_router},
    web::error::AppError,
    web::response::{ApiResponse, LoginResponse},
};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};
use tower::ServiceExt;

#[test]
fn success_response_matches_go_result_envelope() {
    let body = serde_json::to_value(ApiResponse::success(json!({"ok": true}))).unwrap();
    assert_eq!(body, json!({"code": 0, "msg": "", "data": {"ok": true}}));
}

#[test]
fn error_response_matches_go_result_envelope() {
    let body =
        serde_json::to_value(ApiResponse::<serde_json::Value>::error(501, "not ready")).unwrap();
    assert_eq!(body, json!({"code": 501, "msg": "not ready", "data": {}}));
}

#[test]
fn empty_success_response_matches_go_result_envelope() {
    let body = serde_json::to_value(ApiResponse::empty_success()).unwrap();
    assert_eq!(body, json!({"code": 0, "msg": "", "data": {}}));
}

#[test]
fn login_response_preserves_go_login_semantics() {
    let body = serde_json::to_value(LoginResponse::success(json!({"username": "admin"}))).unwrap();
    assert_eq!(
        body,
        json!({"code": 200, "msg": "Login success", "data": {"username": "admin"}})
    );
}

#[test]
fn login_error_response_preserves_go_login_semantics() {
    let body = serde_json::to_value(LoginResponse::<serde_json::Value>::error(
        401,
        "User authentication failed",
    ))
    .unwrap();
    assert_eq!(
        body,
        json!({"code": 401, "msg": "User authentication failed", "data": null})
    );
}

#[tokio::test]
async fn internal_error_response_hides_operational_details() {
    let response = AppError::internal("database path /srv/secrets/dst-db failed").into_response();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(body["code"], 500);
    assert_eq!(body["msg"], "internal server error");
    assert_eq!(body["data"], json!({}));
    assert!(!body.to_string().contains("/srv/secrets"));
}

#[tokio::test]
async fn bad_request_error_response_preserves_safe_client_message() {
    let response = AppError::bad_request("clusterName is required").into_response();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(
        body,
        json!({"code": 400, "msg": "clusterName is required", "data": {}})
    );
}

#[test]
fn logging_writes_plain_text_without_ansi_escape_codes() {
    let dir = tempdir().unwrap();
    let log_path = dir.path().join("dst-admin-go.log");

    logging::init(&log_path).unwrap();
    tracing::info!("plain logging test message");

    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(log.contains("plain logging test message"));
    assert!(
        !log.contains("\u{1b}["),
        "log file contains ANSI escape codes: {log:?}"
    );
}

#[tokio::test]
async fn hello_route_returns_go_plain_text() {
    let (app, _dir) = test_router().await;

    let response = send(&app, Method::GET, "/hello", None, None).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_text(response).await, "Hello! Dont starve together");
}

#[tokio::test]
async fn api_routes_require_session_then_accept_login_cookie_for_kv() {
    let (app, _dir) = test_router().await;

    let unauthorized = send(&app, Method::GET, "/api/kv?key=missing", None, None).await;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let login = send(
        &app,
        Method::POST,
        "/api/login",
        Some(json!({"username": "admin", "password": "123456"})),
        None,
    )
    .await;
    assert_eq!(login.status(), StatusCode::OK);
    let cookie = login_cookie(&login);

    let empty_kv = send(
        &app,
        Method::GET,
        "/api/kv?key=missing",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(empty_kv.status(), StatusCode::OK);
    assert_eq!(
        response_json(empty_kv).await,
        json!({"code": 200, "msg": "success", "data": ""})
    );

    let saved = send(
        &app,
        Method::POST,
        "/api/kv",
        Some(json!({"key": "serverName", "value": "Wendy"})),
        Some(&cookie),
    )
    .await;
    assert_eq!(saved.status(), StatusCode::OK);
    assert_eq!(
        response_json(saved).await,
        json!({"code": 200, "msg": "success", "data": "Wendy"})
    );

    let loaded = send(
        &app,
        Method::GET,
        "/api/kv?key=serverName",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(loaded.status(), StatusCode::OK);
    assert_eq!(
        response_json(loaded).await,
        json!({"code": 200, "msg": "success", "data": "Wendy"})
    );
}

#[tokio::test]
async fn web_link_routes_add_list_and_delete_links() {
    let (app, _dir) = test_router().await;
    let cookie = login(&app).await;

    let created = send(
        &app,
        Method::POST,
        "/api/web/link",
        Some(json!({
            "title": "Panel",
            "url": "https://example.test/panel",
            "width": "1024",
            "height": "768"
        })),
        Some(&cookie),
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    assert_eq!(
        response_json(created).await,
        json!({"code": 200, "msg": "success", "data": null})
    );

    let listed = send(&app, Method::GET, "/api/web/link", None, Some(&cookie)).await;
    assert_eq!(listed.status(), StatusCode::OK);
    let listed_body = response_json(listed).await;
    let links = listed_body["data"].as_array().unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0]["title"], "Panel");
    assert_eq!(links[0]["url"], "https://example.test/panel");
    let id = links[0]["ID"].as_i64().unwrap();

    let deleted = send(
        &app,
        Method::DELETE,
        &format!("/api/web/link?ID={id}"),
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::OK);
    assert_eq!(
        response_json(deleted).await,
        json!({"code": 200, "msg": "success", "data": null})
    );

    let listed = send(&app, Method::GET, "/api/web/link", None, Some(&cookie)).await;
    assert_eq!(response_json(listed).await["data"], json!([]));
}

#[tokio::test]
async fn cluster_collection_routes_are_migrated_from_compatibility_stubs() {
    let (app, _dir) = test_router().await;
    let cookie = login(&app).await;

    for method in [Method::GET, Method::POST, Method::PUT, Method::DELETE] {
        let response = send(&app, method.clone(), "/api/cluster", None, Some(&cookie)).await;
        assert_ne!(response.status(), StatusCode::NOT_IMPLEMENTED, "{method}");
    }
}

#[tokio::test]
async fn root_static_fallback_returns_404_when_dist_index_is_missing() {
    let (app, _dir) = test_router().await;

    let response = send(&app, Method::GET, "/", None, None).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn root_static_fallback_serves_dist_index_when_present() {
    let (app, dir) = test_router().await;
    let dist = dir.path().join("dist");
    fs::create_dir(&dist).unwrap();
    fs::write(dist.join("index.html"), "<main>DST Admin</main>").unwrap();

    let response = send(&app, Method::GET, "/", None, None).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert_eq!(response.headers().get(CACHE_CONTROL).unwrap(), "no-cache");
    assert_eq!(response_text(response).await, "<main>DST Admin</main>");
}

#[tokio::test]
async fn static_asset_routes_serve_files_from_dist_when_present() {
    let (app, dir) = test_router().await;
    let assets = dir.path().join("dist").join("assets");
    fs::create_dir_all(&assets).unwrap();
    fs::write(assets.join("app.js"), "console.log('dst-admin');").unwrap();

    let response = send(&app, Method::GET, "/assets/app.js", None, None).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("application/javascript")
    );
    assert_eq!(
        response.headers().get(CACHE_CONTROL).unwrap(),
        "public, max-age=30672000"
    );
    assert_eq!(response_text(response).await, "console.log('dst-admin');");
}

#[cfg(windows)]
#[tokio::test]
async fn windows_static_asset_routes_serve_hashed_frontend_files() {
    let (app, dir) = test_router().await;
    let assets = dir.path().join("dist").join("assets");
    fs::create_dir_all(&assets).unwrap();
    fs::write(assets.join("index-_7jwq284.css"), "body{}").unwrap();
    fs::write(assets.join("index-Bs3UfZZe.js"), "console.log('ok')").unwrap();

    for (uri, expected) in [
        ("/assets/index-_7jwq284.css", "body{}"),
        ("/assets/index-Bs3UfZZe.js", "console.log('ok')"),
    ] {
        let response = send(&app, Method::GET, uri, None, None).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        assert_eq!(response_text(response).await, expected);
    }
}

#[tokio::test]
async fn static_asset_head_routes_return_headers_without_body() {
    let (app, dir) = test_router().await;
    let assets = dir.path().join("dist").join("assets");
    fs::create_dir_all(&assets).unwrap();
    fs::write(assets.join("app.js"), "console.log('dst-admin');").unwrap();

    let response = send(&app, Method::HEAD, "/assets/app.js", None, None).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("application/javascript")
    );
    assert_eq!(response_text(response).await, "");
}

#[tokio::test]
async fn static_named_files_serve_manifest_and_favicon_from_dist() {
    let (app, dir) = test_router().await;
    let dist = dir.path().join("dist");
    fs::create_dir_all(&dist).unwrap();
    fs::write(dist.join("asset-manifest.json"), r#"{"main":"app.js"}"#).unwrap();
    fs::write(dist.join("favicon.ico"), [0_u8, 1, 2, 3]).unwrap();

    let manifest = send(&app, Method::GET, "/asset-manifest.json", None, None).await;
    assert_eq!(manifest.status(), StatusCode::OK);
    assert!(
        manifest
            .headers()
            .get(CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    assert_eq!(response_text(manifest).await, r#"{"main":"app.js"}"#);

    let favicon = send(&app, Method::GET, "/favicon.ico", None, None).await;
    assert_eq!(favicon.status(), StatusCode::OK);
    assert_eq!(favicon.headers().get(CONTENT_TYPE).unwrap(), "image/x-icon");
    let favicon_bytes = to_bytes(favicon.into_body(), usize::MAX).await.unwrap();
    assert_eq!(favicon_bytes.as_ref(), &[0_u8, 1, 2, 3]);
}

#[tokio::test]
async fn static_asset_routes_reject_path_traversal_attempts() {
    let (app, dir) = test_router().await;
    let dist_assets = dir.path().join("dist").join("assets");
    fs::create_dir_all(&dist_assets).unwrap();
    fs::write(dir.path().join("secret.txt"), "do-not-serve").unwrap();
    fs::write(dist_assets.join("app.js"), "console.log('dst-admin');").unwrap();
    fs::create_dir_all(dist_assets.join("%2e%2e")).unwrap();
    fs::write(
        dist_assets.join("%2e%2e").join("secret.txt"),
        "encoded-literal-should-not-serve",
    )
    .unwrap();

    for uri in [
        "/assets/../secret.txt",
        "/assets/%2e%2e/secret.txt",
        "/assets/..%2fsecret.txt",
    ] {
        let response = send(&app, Method::GET, uri, None, None).await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{uri} should be rejected as a static path escape"
        );
        let body = response_text(response).await;
        assert_ne!(body, "do-not-serve", "{uri} escaped the dist root");
        assert_ne!(
            body, "encoded-literal-should-not-serve",
            "{uri} served a literal encoded traversal fixture"
        );
    }
}

#[tokio::test]
async fn static_asset_routes_return_404_for_directories() {
    let (app, dir) = test_router().await;
    let nested_dir = dir.path().join("dist").join("assets").join("nested");
    fs::create_dir_all(&nested_dir).unwrap();

    let response = send(&app, Method::GET, "/assets/nested", None, None).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[cfg(unix)]
#[tokio::test]
async fn static_asset_routes_reject_symlink_escapes() {
    use std::os::unix::fs::symlink;

    let (app, dir) = test_router().await;
    let dist_assets = dir.path().join("dist").join("assets");
    fs::create_dir_all(&dist_assets).unwrap();
    fs::write(dir.path().join("secret.txt"), "do-not-serve").unwrap();
    symlink(
        dir.path().join("secret.txt"),
        dist_assets.join("secret-link.txt"),
    )
    .unwrap();

    let response = send(&app, Method::GET, "/assets/secret-link.txt", None, None).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[cfg(unix)]
#[tokio::test]
async fn root_static_fallback_rejects_symlinked_index_escape() {
    use std::os::unix::fs::symlink;

    let (app, dir) = test_router().await;
    let dist = dir.path().join("dist");
    fs::create_dir_all(&dist).unwrap();
    fs::write(
        dir.path().join("secret-index.html"),
        "<main>do-not-serve</main>",
    )
    .unwrap();
    symlink(
        dir.path().join("secret-index.html"),
        dist.join("index.html"),
    )
    .unwrap();

    let response = send(&app, Method::GET, "/", None, None).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn login_route_ignores_spoofed_forwarded_headers_for_white_admin_ip() {
    let (state, _dir) = test_state_with_white_admin_ip("10.1.0.0/16").await;
    let app = build_router(state);

    let response = send_with_headers(
        &app,
        Method::POST,
        "/api/login",
        Some(json!({"username": "ignored", "password": "wrong-password"})),
        None,
        &[
            ("x-forwarded-for", "10.1.2.3"),
            ("x-real-ip", "10.1.2.3"),
            ("forwarded", "for=10.1.2.3"),
        ],
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get(SET_COOKIE).is_none());
    assert_eq!(
        response_json(response).await,
        json!({"code": 401, "msg": "User authentication failed", "data": null})
    );
}

#[tokio::test]
async fn init_route_reports_first_run_status_from_root_first_file() {
    let (app, dir) = test_router().await;

    let missing = send(&app, Method::GET, "/api/init", None, None).await;
    assert_eq!(missing.status(), StatusCode::OK);
    assert_eq!(
        response_json(missing).await,
        json!({"code": 200, "msg": "is first", "data": null})
    );

    fs::write(dir.path().join("first"), "").unwrap();
    let present = send(&app, Method::GET, "/api/init", None, None).await;
    assert_eq!(present.status(), StatusCode::OK);
    assert_eq!(
        response_json(present).await,
        json!({"code": 400, "msg": "is not first", "data": null})
    );
}

#[tokio::test]
async fn app_router_can_be_wrapped_as_connect_info_make_service() {
    let (state, _dir) = test_state().await;

    let _make_service = build_connect_info_service(state);
    let _peer_addr: SocketAddr = "127.0.0.1:3000".parse().unwrap();
}

async fn test_router() -> (Router, TempDir) {
    let (state, dir) = test_state().await;
    (build_router(state), dir)
}

async fn test_state() -> (AppState, TempDir) {
    test_state_with_white_admin_ip_option(None).await
}

async fn test_state_with_white_admin_ip(white_admin_ip: &str) -> (AppState, TempDir) {
    test_state_with_white_admin_ip_option(Some(white_admin_ip.to_owned())).await
}

async fn test_state_with_white_admin_ip_option(
    white_admin_ip: Option<String>,
) -> (AppState, TempDir) {
    let dir = tempdir().unwrap();
    write_password_file(dir.path());
    let pool = connect_sqlite_memory().await.unwrap();
    migrate(&pool).await.unwrap();
    let mut config = test_config();
    config.white_admin_ip = white_admin_ip;
    let state = AppState::new(config, pool, SessionStore::new(), dir.path());

    (state, dir)
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
        token: None,
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
    login_cookie(&response)
}

fn login_cookie(response: &Response<Body>) -> String {
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

async fn send_with_headers(
    app: &Router,
    method: Method,
    uri: &str,
    json_body: Option<Value>,
    cookie: Option<&str>,
    headers: &[(&str, &str)],
) -> Response<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if json_body.is_some() {
        builder = builder.header(CONTENT_TYPE, "application/json");
    }
    if let Some(cookie) = cookie {
        builder = builder.header(COOKIE, cookie);
    }
    for (name, value) in headers {
        builder = builder.header(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
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
