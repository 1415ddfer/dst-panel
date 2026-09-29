//! DST dedicated server update command helpers.

use super::*;

/// Updates the DST dedicated server installation.
pub(crate) async fn update_game(
    runner: &dyn CommandRunner,
    process_provider: &dyn ProcessSnapshotProvider,
    root: &Path,
    context: &LifecycleContext,
    grace_period: Duration,
) -> AppResult<()> {
    if context.config.bin == 2664 {
        ensure_no_levels_running(process_provider, root, context)?;
    } else {
        super::start_stop::stop_all_strict(runner, process_provider, root, context, grace_period)
            .await?;
    }
    let spec = update_spec(&context.config)?;
    let output = runner
        .run(spec)
        .await
        .map_err(command_error("update-game"))?;
    if output.status_code != Some(0) {
        tracing::warn!(
            status_code = ?output.status_code,
            stdout_len = output.stdout.len(),
            stderr_len = output.stderr.len(),
            "DST update command exited unsuccessfully"
        );
        return Err(AppError::internal("update game"));
    }
    if context.config.bin != 2664 {
        rewrite_dedicated_server_mods_setup(root, context)?;
    }
    tracing::info!(cluster_name = %context.cluster_name, "updated DST installation");
    Ok(())
}

fn ensure_no_levels_running(
    process_provider: &dyn ProcessSnapshotProvider,
    root: &Path,
    context: &LifecycleContext,
) -> AppResult<()> {
    let snapshots = process_provider.snapshots().map_err(|error| {
        tracing::warn!(
            cluster_name = %context.cluster_name,
            error = %error,
            "failed to collect process snapshots before bin=2664 update"
        );
        AppError::internal("collect process snapshots")
    })?;
    for level_name in context.level_names(root)? {
        if first_level_process(&snapshots, &context.cluster_name, &level_name).is_some() {
            tracing::error!(
                cluster_name = %context.cluster_name,
                level_name,
                "refusing bin=2664 update while DST shard is running"
            );
            return Err(AppError::internal("update game"));
        }
    }
    Ok(())
}

fn rewrite_dedicated_server_mods_setup(root: &Path, context: &LifecycleContext) -> AppResult<()> {
    let cluster_dir = dst::cluster_dir(root, &context.cluster_name)
        .map_err(file_error("resolve cluster directory"))?;
    for level_name in context.level_names(root)? {
        let modoverrides = dst::safe_read_cluster_file_to_string(
            &cluster_dir,
            Path::new(&level_name).join("modoverrides.lua"),
        )
        .map_err(file_error("read level modoverrides"))?
        .unwrap_or_default();
        mod_setup::merge_dedicated_server_mods_setup(root, &modoverrides)
            .map_err(file_error("write dedicated_server_mods_setup.lua"))?;
    }
    Ok(())
}

fn update_spec(config: &DstConfig) -> AppResult<CommandSpec> {
    if config.bin == 2664 {
        return Ok(CommandSpec::new("./DepotDownloader")
            .with_current_dir("/opt/DepotDownloader")
            .arg("-app")
            .arg("343050")
            .arg("-os")
            .arg("linux")
            .arg("-osarch")
            .arg("64")
            .arg("-dir")
            .arg(install_dir(config).display().to_string())
            .arg("-validate")
            .with_timeout(COMMAND_TIMEOUT));
    }

    let (current_dir, program, prefix_args) = steamcmd_invocation(config)?;
    let mut spec = CommandSpec::new(program)
        .with_current_dir(current_dir)
        .extend_args(prefix_args)
        .arg("+login")
        .arg("anonymous")
        .arg("+force_install_dir")
        .arg(install_dir(config).display().to_string())
        .arg("+app_update")
        .arg("343050");
    if config.beta == 1 {
        spec = spec.arg("-beta").arg("updatebeta");
    }
    Ok(spec
        .arg("validate")
        .arg("+quit")
        .with_timeout(COMMAND_TIMEOUT))
}

fn steamcmd_invocation(config: &DstConfig) -> AppResult<(std::path::PathBuf, String, Vec<String>)> {
    if config.steamcmd.contains('\0') {
        return Err(AppError::bad_request(
            "steamcmd path contains unsafe characters",
        ));
    }
    let steamcmd_dir = Path::new(&config.steamcmd);
    if cfg!(windows) {
        return Ok((
            steamcmd_dir.to_path_buf(),
            steamcmd_dir.join("steamcmd.exe").display().to_string(),
            Vec::new(),
        ));
    }
    if config.bin == 86 {
        return Ok((
            steamcmd_dir.to_path_buf(),
            "box86".to_owned(),
            vec!["./linux32/steamcmd".to_owned()],
        ));
    }
    let script = steamcmd_dir.join("steamcmd.sh");
    if script.exists() {
        Ok((
            steamcmd_dir.to_path_buf(),
            "./steamcmd.sh".to_owned(),
            Vec::new(),
        ))
    } else {
        Ok((
            steamcmd_dir.to_path_buf(),
            "./steamcmd".to_owned(),
            Vec::new(),
        ))
    }
}

fn install_dir(config: &DstConfig) -> std::path::PathBuf {
    if config.beta == 1 {
        std::path::PathBuf::from(format!("{}-beta", config.force_install_dir))
    } else {
        std::path::PathBuf::from(&config.force_install_dir)
    }
}
