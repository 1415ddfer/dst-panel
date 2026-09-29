//! Installation handlers for SteamCMD setup.
//!
//! The legacy Go route streams Server-Sent Events while installing OS
//! dependencies and SteamCMD. Running package-manager shell snippets from a
//! web request is intentionally not reproduced in Rust. This handler keeps the
//! route shape and SteamCMD script invocation, but goes through the argv-based
//! [`crate::infra::command::CommandRunner`] boundary so tests can prove no user input
//! reaches a shell string.

use std::{
    fs, io,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

#[cfg(windows)]
use std::io::Read;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use axum::{
    body::Body,
    extract::State,
    http::{
        HeaderMap, StatusCode,
        header::{ACCESS_CONTROL_ALLOW_ORIGIN, CACHE_CONTROL, CONNECTION, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
};

use crate::{
    dst::DstConfig,
    infra::command::{CommandOutput, CommandSpec},
    web::app::AppState,
    web::handlers::{auth::extract_token_cookie, init},
};

const INSTALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
#[cfg(windows)]
const WINDOWS_STEAMCMD_URL: &str = "https://steamcdn-a.akamaihd.net/client/installer/steamcmd.zip";
static INSTALL_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// Installs SteamCMD and returns Go-compatible SSE event text.
pub(crate) async fn install_steamcmd_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if init::path_entry_exists(&state.root_path.join("first"))
        && !has_valid_session(&state, &headers)
    {
        tracing::warn!("rejected unauthenticated SteamCMD installation after first-run");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let Some(_guard) = InstallGuard::try_acquire() else {
        tracing::warn!("rejected concurrent SteamCMD installation request");
        return build_sse_response("data: 安装任务正在进行中\n\ndata: end\n\n".to_owned());
    };

    let mut events = String::new();
    append_event(&mut events, "正在安装steamcmd。。。");

    #[cfg(windows)]
    let spec = match prepare_windows_steamcmd(&state).await {
        Ok(spec) => spec,
        Err(error) => {
            append_event(&mut events, "安装steamcmd失败！！！");
            append_event(&mut events, "end");
            tracing::warn!(error = %error, "failed to prepare Windows SteamCMD installation");
            return build_sse_response(events);
        }
    };

    #[cfg(not(windows))]
    let spec = {
        let root_path = state.root_path.display().to_string();
        let script_path = state
            .root_path
            .join("static")
            .join("script")
            .join("install_steamcmd.sh");
        if let Err(error) = ensure_script_executable(&script_path) {
            append_event(&mut events, "安装steamcmd失败！！！");
            append_event(&mut events, "end");
            tracing::warn!(
                script_path = %script_path.display(),
                error = %error,
                "failed to prepare SteamCMD install script"
            );
            return build_sse_response(events);
        }

        CommandSpec::new(script_path.display().to_string())
            .arg(root_path.clone())
            .arg(root_path)
            .with_timeout(INSTALL_TIMEOUT)
    };

    tracing::info!("starting SteamCMD installation script");
    match state.command_runner.run(spec).await {
        Ok(output) if command_succeeded(&output) => {
            append_output_events(&mut events, &output);
            match persist_install_dst_config(&state) {
                Ok(()) => {
                    append_event(&mut events, "[successed]");
                    append_event(&mut events, "end");
                    tracing::info!(
                        status_code = ?output.status_code,
                        stdout_len = output.stdout.len(),
                        stderr_len = output.stderr.len(),
                        "SteamCMD installation script completed"
                    );
                }
                Err(error) => {
                    append_event(&mut events, "安装steamcmd失败！！！");
                    append_event(&mut events, "end");
                    tracing::warn!(
                        error = %error,
                        "SteamCMD installation succeeded but config persistence failed"
                    );
                }
            }
        }
        Ok(output) => {
            append_output_events(&mut events, &output);
            append_event(&mut events, "安装steamcmd失败！！！");
            append_event(&mut events, "end");
            tracing::warn!(
                status_code = ?output.status_code,
                timed_out = output.timed_out,
                stdout_len = output.stdout.len(),
                stderr_len = output.stderr.len(),
                "SteamCMD installation script failed"
            );
        }
        Err(error) => {
            append_event(&mut events, "安装steamcmd失败！！！");
            append_event(&mut events, "end");
            tracing::warn!(error = %error, "SteamCMD installation command failed before output");
        }
    }

    build_sse_response(events)
}

#[cfg(windows)]
async fn prepare_windows_steamcmd(state: &AppState) -> io::Result<CommandSpec> {
    use crate::infra::{
        fs_paths::{safe_ensure_dir_path, safe_overwrite_file_path},
        http_client::HttpRequest,
    };

    let steamcmd_dir = state.root_path.join("steamcmd");
    let program = steamcmd_dir.join("steamcmd.exe");
    if !program.is_file() {
        let response = state
            .http_client
            .send(HttpRequest::new("GET", WINDOWS_STEAMCMD_URL))
            .await
            .map_err(io::Error::other)?;
        if response.status != 200 {
            return Err(io::Error::other("SteamCMD download failed"));
        }
        let mut archive =
            zip::ZipArchive::new(io::Cursor::new(response.body)).map_err(io::Error::other)?;
        let mut entry = archive.by_name("steamcmd.exe").map_err(io::Error::other)?;
        let mut bytes = Vec::new();
        entry.take(25 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.is_empty() || bytes.len() > 25 * 1024 * 1024 {
            return Err(io::Error::other("SteamCMD executable size is invalid"));
        }
        safe_ensure_dir_path(&steamcmd_dir).map_err(io::Error::other)?;
        safe_overwrite_file_path(&program, &bytes).map_err(io::Error::other)?;
    }
    Ok(CommandSpec::new(program.display().to_string())
        .with_current_dir(steamcmd_dir)
        .arg("+force_install_dir")
        .arg(
            state
                .root_path
                .join("dst-dedicated-server")
                .display()
                .to_string(),
        )
        .arg("+login")
        .arg("anonymous")
        .arg("+app_update")
        .arg("343050")
        .arg("validate")
        .arg("+quit")
        .with_timeout(INSTALL_TIMEOUT))
}

fn build_sse_response(events: String) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(CACHE_CONTROL, "no-cache")
        .header(CONNECTION, "keep-alive")
        .header(ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Body::from(events))
        .expect("static SSE response is valid")
}

fn has_valid_session(state: &AppState, headers: &HeaderMap) -> bool {
    extract_token_cookie(headers)
        .filter(|session_id| !session_id.is_empty())
        .and_then(|session_id| state.sessions.validate(&session_id))
        .is_some()
}

struct InstallGuard;

impl InstallGuard {
    fn try_acquire() -> Option<Self> {
        INSTALL_IN_PROGRESS
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Self)
    }
}

impl Drop for InstallGuard {
    fn drop(&mut self) {
        INSTALL_IN_PROGRESS.store(false, Ordering::Release);
    }
}

fn ensure_script_executable(script_path: &Path) -> io::Result<()> {
    let metadata = fs::metadata(script_path)?;
    #[cfg(unix)]
    {
        let mut permissions = metadata.permissions();
        let mode = permissions.mode();
        if mode & 0o100 == 0 {
            permissions.set_mode(mode | 0o100);
            fs::set_permissions(script_path, permissions)?;
        }
    }
    Ok(())
}

fn persist_install_dst_config(state: &AppState) -> io::Result<()> {
    let klei_root = state.root_path.join(".klei").join("DoNotStarveTogether");
    DstConfig {
        steamcmd: state.root_path.join("steamcmd").display().to_string(),
        force_install_dir: state
            .root_path
            .join("dst-dedicated-server")
            .display()
            .to_string(),
        backup: klei_root.display().to_string(),
        mod_download_path: klei_root.display().to_string(),
        cluster: init::DEFAULT_FIRST_RUN_CLUSTER.to_owned(),
        ..DstConfig::default()
    }
    .save_with_fallbacks(&state.root_path)?;
    init::initialize_default_cluster(&state.root_path, None)
        .map_err(|error| io::Error::other(error.to_string()))?;
    tracing::info!("persisted SteamCMD installation dst_config defaults");
    Ok(())
}

fn command_succeeded(output: &CommandOutput) -> bool {
    !output.timed_out && output.status_code == Some(0)
}

fn append_output_events(events: &mut String, output: &CommandOutput) {
    append_utf8_lines(events, &output.stdout);
    append_utf8_lines(events, &output.stderr);
    if output.stdout_truncated {
        append_event(events, "[stdout truncated]");
    }
    if output.stderr_truncated {
        append_event(events, "[stderr truncated]");
    }
}

fn append_utf8_lines(events: &mut String, bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    for line in text.lines().filter(|line| !line.is_empty()) {
        append_event(events, line);
    }
}

fn append_event(events: &mut String, event: &str) {
    events.push_str("data: ");
    events.push_str(event);
    events.push_str("\n\n");
}
