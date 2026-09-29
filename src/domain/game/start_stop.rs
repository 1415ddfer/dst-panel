//! DST shard start and stop command helpers.

use super::*;

const MASTER_LEVELDATAOVERRIDE: &str = include_str!("../../../static/Master/leveldataoverride.lua");
const CAVES_LEVELDATAOVERRIDE: &str = include_str!("../../../static/Caves/leveldataoverride.lua");

/// Starts one DST shard.
pub(crate) async fn start_level(
    runner: &dyn CommandRunner,
    process_provider: &dyn ProcessSnapshotProvider,
    root: &Path,
    context: &LifecycleContext,
    level_name: &str,
    grace_period: Duration,
) -> AppResult<()> {
    let level_name = validate_safe_command_arg("level name", level_name)
        .map_err(|error| AppError::bad_request(error.to_string()))?
        .into_string();
    stop_level(runner, process_provider, context, &level_name, grace_period).await?;
    launch_level(runner, root, context, &level_name).await
}

/// Requests graceful shutdown for one DST shard.
pub(crate) async fn stop_level(
    runner: &dyn CommandRunner,
    process_provider: &dyn ProcessSnapshotProvider,
    context: &LifecycleContext,
    level_name: &str,
    grace_period: Duration,
) -> AppResult<()> {
    let level_name = validate_safe_command_arg("level name", level_name)
        .map_err(|error| AppError::bad_request(error.to_string()))?
        .into_string();
    stop_level_inner(
        runner,
        process_provider,
        context,
        &level_name,
        grace_period,
        false,
    )
    .await
}

async fn stop_level_strict(
    runner: &dyn CommandRunner,
    process_provider: &dyn ProcessSnapshotProvider,
    context: &LifecycleContext,
    level_name: &str,
    grace_period: Duration,
) -> AppResult<()> {
    let level_name = validate_safe_command_arg("level name", level_name)
        .map_err(|error| AppError::bad_request(error.to_string()))?
        .into_string();
    stop_level_inner(
        runner,
        process_provider,
        context,
        &level_name,
        grace_period,
        true,
    )
    .await
}

async fn stop_level_inner(
    runner: &dyn CommandRunner,
    process_provider: &dyn ProcessSnapshotProvider,
    context: &LifecycleContext,
    level_name: &str,
    grace_period: Duration,
    strict: bool,
) -> AppResult<()> {
    let spec = screen_command_spec(&context.cluster_name, level_name, "c_shutdown(true)");
    run_go_lenient(
        runner,
        spec,
        "stop-level",
        &context.cluster_name,
        level_name,
    )
    .await;
    if !grace_period.is_zero() {
        tokio::time::sleep(grace_period).await;
    }
    let kill_result =
        kill_level_if_still_running(runner, process_provider, context, level_name).await;
    if strict {
        kill_result?;
        ensure_level_stopped(process_provider, context, level_name)?;
    } else if let Err(error) = kill_result {
        tracing::warn!(
            cluster_name = %context.cluster_name,
            level_name,
            error = %error,
            "DST hard-kill fallback failed; preserving Go stop response"
        );
    }
    Ok(())
}

/// Starts all indexed shards in their `level.json` order.
pub(crate) async fn start_all(
    runner: &dyn CommandRunner,
    process_provider: &dyn ProcessSnapshotProvider,
    root: &Path,
    context: &LifecycleContext,
    grace_period: Duration,
) -> AppResult<()> {
    let level_names = context.level_names(root)?;
    for level_name in &level_names {
        stop_level(runner, process_provider, context, level_name, grace_period).await?;
    }
    for level_name in &level_names {
        launch_level(runner, root, context, level_name).await?;
    }
    Ok(())
}

/// Stops all indexed shards in their `level.json` order.
pub(crate) async fn stop_all(
    runner: &dyn CommandRunner,
    process_provider: &dyn ProcessSnapshotProvider,
    root: &Path,
    context: &LifecycleContext,
    grace_period: Duration,
) -> AppResult<()> {
    for level_name in context.level_names(root)? {
        stop_level(runner, process_provider, context, &level_name, grace_period).await?;
    }
    Ok(())
}

/// Stops all shards and verifies none remain before callers mutate archives or installs.
pub(crate) async fn stop_all_strict(
    runner: &dyn CommandRunner,
    process_provider: &dyn ProcessSnapshotProvider,
    root: &Path,
    context: &LifecycleContext,
    grace_period: Duration,
) -> AppResult<()> {
    for level_name in context.level_names(root)? {
        stop_level_strict(runner, process_provider, context, &level_name, grace_period).await?;
    }
    Ok(())
}

/// Mirrors Go's `startBefore` helper for the single-level start route.
pub(crate) fn copy_steamclient_before_single_start(
    root: &Path,
    context: &LifecycleContext,
) -> AppResult<()> {
    #[cfg(windows)]
    {
        let _ = (root, context);
        return Ok(());
    }

    #[cfg(not(windows))]
    {
        match copy_steamclient_before_single_start_inner(root, &context.config) {
            Ok(()) => {
                tracing::info!(
                    cluster_name = %context.cluster_name,
                    "copied steamclient.so before single DST start"
                );
            }
            Err(error) => {
                // Go logs these copy failures and still starts the shard. Preserve
                // that route contract while making the skipped side effect visible.
                tracing::warn!(
                    cluster_name = %context.cluster_name,
                    error = %error,
                    "failed to copy steamclient.so before single DST start"
                );
            }
        }
        Ok(())
    }
}

async fn launch_level(
    runner: &dyn CommandRunner,
    root: &Path,
    context: &LifecycleContext,
    level_name: &str,
) -> AppResult<()> {
    ensure_level_files_before_launch(root, context, level_name)?;
    let spec = launch_level_spec(root, context, level_name)?;
    #[cfg(windows)]
    return run_go_strict(
        runner,
        spec,
        "start-level",
        &context.cluster_name,
        level_name,
    )
    .await;

    #[cfg(not(windows))]
    {
        run_go_lenient(
            runner,
            spec,
            "start-level",
            &context.cluster_name,
            level_name,
        )
        .await;
        Ok(())
    }
}

fn ensure_level_files_before_launch(
    root: &Path,
    context: &LifecycleContext,
    level_name: &str,
) -> AppResult<()> {
    let cluster_dir = dst::cluster_dir(root, &context.cluster_name)
        .map_err(file_error("resolve cluster directory"))?;
    ensure_cluster_ini_before_launch(&cluster_dir)?;
    let worlds = level::list_existing_worlds_from_cluster_dir(&cluster_dir)?;
    let world = worlds
        .iter()
        .find(|world| world.uuid.eq_ignore_ascii_case(level_name));
    let (leveldataoverride, repair_leveldataoverride) =
        leveldataoverride_for_launch(&cluster_dir, level_name)?;
    let (modoverrides, server_ini) = if let Some(world) = world {
        (world.modoverrides.clone(), world.server_ini.clone())
    } else {
        (
            "return {}".to_owned(),
            default_server_ini_for_missing_level(level_name, &worlds),
        )
    };
    if repair_leveldataoverride {
        dst::safe_write_cluster_file(
            &cluster_dir,
            Path::new(level_name).join("leveldataoverride.lua"),
            &leveldataoverride,
        )
        .map_err(file_error("repair leveldataoverride"))?;
    }

    dst::write_world_files_if_missing(
        &cluster_dir,
        level_name,
        &leveldataoverride,
        &modoverrides,
        &server_ini,
    )
    .map_err(file_error("repair level files"))?;
    Ok(())
}

fn ensure_cluster_ini_before_launch(cluster_dir: &Path) -> AppResult<()> {
    let contents = dst::safe_read_cluster_file_to_string(cluster_dir, "cluster.ini")
        .map_err(file_error("read cluster.ini"))?;
    let Some(contents) = contents else {
        return dst::safe_write_cluster_file(
            cluster_dir,
            "cluster.ini",
            dst::cluster_ini::ClusterIni::default_for_new_cluster().to_ini(),
        )
        .map_err(file_error("write cluster.ini"));
    };
    if !cluster_ini_needs_game_mode_repair(&contents) {
        return Ok(());
    }
    let cluster_ini = dst::cluster_ini::ClusterIni::from_contents(&contents);
    dst::safe_write_cluster_file(cluster_dir, "cluster.ini", cluster_ini.to_ini())
        .map_err(file_error("repair cluster.ini"))
}

fn cluster_ini_needs_game_mode_repair(contents: &str) -> bool {
    contents
        .lines()
        .filter_map(|line| line.trim().split_once('='))
        .find(|(key, _)| key.trim() == "game_mode")
        .map(|(_, value)| value.trim().is_empty())
        .unwrap_or(true)
}

fn leveldataoverride_for_launch(cluster_dir: &Path, level_name: &str) -> AppResult<(String, bool)> {
    let existing = dst::safe_read_cluster_file_to_string(
        cluster_dir,
        Path::new(level_name).join("leveldataoverride.lua"),
    )
    .map_err(file_error("read leveldataoverride"))?;
    let default = default_leveldataoverride_for_level(level_name).to_owned();
    match existing {
        Some(contents) if should_repair_leveldataoverride(level_name, &contents) => {
            Ok((default, true))
        }
        Some(contents) if !contents.trim().is_empty() => Ok((contents, false)),
        _ => Ok((default, false)),
    }
}

fn default_leveldataoverride_for_level(level_name: &str) -> &'static str {
    if level_name.eq_ignore_ascii_case("Master") {
        MASTER_LEVELDATAOVERRIDE
    } else {
        CAVES_LEVELDATAOVERRIDE
    }
}

fn should_repair_leveldataoverride(level_name: &str, contents: &str) -> bool {
    !level_name.eq_ignore_ascii_case("Master") && is_empty_lua_table(contents)
}

fn is_empty_lua_table(contents: &str) -> bool {
    contents
        .chars()
        .filter(|value| !value.is_whitespace())
        .collect::<String>()
        == "return{}"
}

fn default_server_ini_for_missing_level(
    level_name: &str,
    worlds: &[level::World],
) -> dst::server_ini::ServerIni {
    if level_name.eq_ignore_ascii_case("Master") {
        return dst::server_ini::ServerIni::master_default();
    }

    let mut server_ini = dst::server_ini::ServerIni::caves_default();
    server_ini.name = level_name.to_owned();
    if !level_name.eq_ignore_ascii_case("Caves") {
        server_ini.server_port = next_level_port(worlds, |ini| ini.server_port, 10998);
        server_ini.authentication_port =
            next_level_port(worlds, |ini| ini.authentication_port, 8766);
        server_ini.master_server_port =
            next_level_port(worlds, |ini| ini.master_server_port, 27016);
    }
    server_ini
}

fn next_level_port(
    worlds: &[level::World],
    field: impl Fn(&dst::server_ini::ServerIni) -> u64,
    fallback: u64,
) -> u64 {
    worlds
        .iter()
        .map(|world| field(&world.server_ini))
        .max()
        .unwrap_or(fallback - 1)
        + 1
}

fn launch_level_spec(
    root: &Path,
    context: &LifecycleContext,
    level_name: &str,
) -> AppResult<CommandSpec> {
    let (current_dir, binary_args) = binary_and_wrapper_args(&context.config)?;
    let mut spec = CommandSpec::new(SCREEN_PROGRAM)
        .arg("-d")
        .arg("-m")
        .arg("-S")
        .arg(screen_session_key(&context.cluster_name, level_name))
        .with_current_dir(current_dir);
    for arg in binary_args {
        spec = spec.arg(arg);
    }
    spec = spec
        .arg("-console")
        .arg("-cluster")
        .arg(&context.cluster_name)
        .arg("-shard")
        .arg(level_name);
    if !context.config.ugc_directory.is_empty() {
        spec = spec
            .arg("-ugc_directory")
            .arg(&context.config.ugc_directory);
    }
    if !context.config.persistent_storage_root.is_empty() {
        spec = spec
            .arg("-persistent_storage_root")
            .arg(&context.config.persistent_storage_root);
    } else if cfg!(windows) {
        spec = spec
            .arg("-persistent_storage_root")
            .arg(root.join(".klei").display().to_string());
    }
    if !context.config.conf_dir.is_empty() {
        spec = spec.arg("-conf_dir").arg(&context.config.conf_dir);
    }
    Ok(spec.with_timeout(COMMAND_TIMEOUT))
}

async fn kill_level_if_still_running(
    runner: &dyn CommandRunner,
    process_provider: &dyn ProcessSnapshotProvider,
    context: &LifecycleContext,
    level_name: &str,
) -> AppResult<bool> {
    let snapshots = match process_provider.snapshots() {
        Ok(snapshots) => snapshots,
        Err(error) => {
            tracing::warn!(
                cluster_name = %context.cluster_name,
                level_name,
                error = %error,
                "failed to collect process snapshots before DST kill fallback"
            );
            return Err(AppError::internal("collect process snapshots"));
        }
    };
    let Some(process) = first_level_process(&snapshots, &context.cluster_name, level_name) else {
        tracing::debug!(
            cluster_name = %context.cluster_name,
            level_name,
            "skipped DST kill fallback because no matching process remained"
        );
        return Ok(false);
    };
    let Some(pid) = process.pid else {
        tracing::warn!(
            cluster_name = %context.cluster_name,
            level_name,
            "cannot hard-kill DST shard because process snapshot has no pid"
        );
        return Err(AppError::internal("kill level"));
    };
    let spec = kill_level_spec(pid);
    run_go_strict(
        runner,
        spec,
        "kill-level",
        &context.cluster_name,
        level_name,
    )
    .await?;
    Ok(true)
}

fn kill_level_spec(pid: u32) -> CommandSpec {
    #[cfg(windows)]
    return CommandSpec::new("taskkill.exe")
        .arg("/PID")
        .arg(pid.to_string())
        .arg("/T")
        .arg("/F")
        .with_timeout(Duration::from_secs(10));

    #[cfg(not(windows))]
    {
        CommandSpec::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .with_timeout(Duration::from_secs(10))
    }
}

fn ensure_level_stopped(
    process_provider: &dyn ProcessSnapshotProvider,
    context: &LifecycleContext,
    level_name: &str,
) -> AppResult<()> {
    let snapshots = process_provider.snapshots().map_err(|error| {
        tracing::warn!(
            cluster_name = %context.cluster_name,
            level_name,
            error = %error,
            "failed to verify DST shard stopped"
        );
        AppError::internal("verify level stopped")
    })?;
    if first_level_process(&snapshots, &context.cluster_name, level_name).is_some() {
        tracing::error!(
            cluster_name = %context.cluster_name,
            level_name,
            "DST shard remained after stop barrier"
        );
        return Err(AppError::internal("stop level"));
    }
    Ok(())
}

fn binary_and_wrapper_args(config: &DstConfig) -> AppResult<(std::path::PathBuf, Vec<String>)> {
    let install_dir = install_dir(config);
    #[cfg(windows)]
    {
        let (directory, executable) = if config.bin == 32 {
            ("bin", "dontstarve_dedicated_server_nullrenderer.exe")
        } else {
            ("bin64", "dontstarve_dedicated_server_nullrenderer_x64.exe")
        };
        return Ok((install_dir.join(directory), vec![executable.to_owned()]));
    }

    #[cfg(not(windows))]
    {
        let bin64 = install_dir.join("bin64");
        let bin32 = install_dir.join("bin");
        let (current_dir, args) = match config.bin {
            64 => (
                bin64,
                vec!["./dontstarve_dedicated_server_nullrenderer_x64".to_owned()],
            ),
            100 => (
                bin64,
                vec!["./dontstarve_dedicated_server_nullrenderer_x64_luajit".to_owned()],
            ),
            86 => (
                bin64,
                vec![
                    "box86".to_owned(),
                    "./dontstarve_dedicated_server_nullrenderer_x64".to_owned(),
                ],
            ),
            2664 => (
                bin64,
                vec![
                    "box64".to_owned(),
                    "./dontstarve_dedicated_server_nullrenderer_x64".to_owned(),
                ],
            ),
            _ => (
                bin32,
                vec!["./dontstarve_dedicated_server_nullrenderer".to_owned()],
            ),
        };
        if args.iter().any(|arg| arg.contains('\0')) {
            return Err(AppError::bad_request(
                "install path contains unsafe characters",
            ));
        }
        Ok((current_dir, args))
    }
}
