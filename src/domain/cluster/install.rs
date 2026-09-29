//! Go-compatible DST dedicated server installation.
//!
//! The Go cluster-create route runs SteamCMD before writing the file skeleton
//! when `force_install_dir` is missing. Rust preserves that side effect through
//! [`crate::infra::command::CommandRunner`] and argv arrays, rather than rebuilding
//! Go's shell-shaped command string. This keeps user-configured paths out of a
//! shell parser while preserving the working directory and argument contract.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{
    infra::command::{CommandOutput, CommandRunner, CommandSpec},
    web::error::{AppError, AppResult},
};

const DST_APP_ID: &str = "343050";
const INSTALL_TIMEOUT: Duration = Duration::from_secs(60 * 60);

pub(crate) async fn install_dedicated_server_if_missing(
    runner: &dyn CommandRunner,
    cluster_name: &str,
    steam_cmd: &str,
    force_install_dir: &str,
) -> AppResult<()> {
    let install_dir = Path::new(force_install_dir);
    if install_dir.exists() {
        tracing::info!(
            cluster_name,
            force_install_dir = %install_dir.display(),
            "skipping DST install because force_install_dir already exists"
        );
        return Ok(());
    }

    tracing::info!(
        cluster_name,
        steamcmd = steam_cmd,
        force_install_dir = %install_dir.display(),
        app_id = DST_APP_ID,
        "starting DST dedicated server install"
    );
    let spec = install_spec(Path::new(steam_cmd), install_dir)?;
    let output = runner.run(spec).await.map_err(|error| {
        tracing::error!(
            cluster_name,
            steamcmd = steam_cmd,
            force_install_dir = %install_dir.display(),
            error = %error,
            "failed to run DST dedicated server install command"
        );
        AppError::internal("install DST dedicated server")
    })?;

    if !command_succeeded(&output) {
        tracing::error!(
            cluster_name,
            steamcmd = steam_cmd,
            force_install_dir = %install_dir.display(),
            status_code = ?output.status_code,
            timed_out = output.timed_out,
            stdout_len = output.stdout.len(),
            stderr_len = output.stderr.len(),
            "DST dedicated server install command failed"
        );
        return Err(AppError::internal("install DST dedicated server"));
    }

    tracing::info!(
        cluster_name,
        steamcmd = steam_cmd,
        force_install_dir = %install_dir.display(),
        "finished DST dedicated server install"
    );
    Ok(())
}

fn install_spec(steam_cmd: &Path, force_install_dir: &Path) -> AppResult<CommandSpec> {
    reject_nul_path("steamcmd", steam_cmd)?;
    reject_nul_path("force_install_dir", force_install_dir)?;

    let program = if cfg!(windows) {
        "steamcmd.exe"
    } else {
        "./steamcmd.sh"
    };
    Ok(CommandSpec::new(program)
        .with_current_dir(PathBuf::from(steam_cmd))
        .arg("+login")
        .arg("anonymous")
        .arg("+force_install_dir")
        .arg(force_install_dir.display().to_string())
        .arg("+app_update")
        .arg(DST_APP_ID)
        .arg("validate")
        .arg("+quit")
        .with_timeout(INSTALL_TIMEOUT))
}

fn command_succeeded(output: &CommandOutput) -> bool {
    !output.timed_out && output.status_code == Some(0)
}

fn reject_nul_path(name: &'static str, path: &Path) -> AppResult<()> {
    let value = path.display().to_string();
    if value.contains('\0') {
        return Err(AppError::bad_request(format!(
            "{name} path contains unsafe characters"
        )));
    }
    Ok(())
}
