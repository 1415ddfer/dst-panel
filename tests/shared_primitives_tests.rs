use std::{
    fs,
    io::Write,
    time::{Duration, Instant},
};

#[cfg(windows)]
use std::io::Read;

#[cfg(windows)]
use dst_admin_rust::infra::process::{ProcessSnapshotProvider, SystemProcessSnapshotProvider};
use dst_admin_rust::{
    infra::command::{
        CommandError, CommandOutput, CommandRunner, CommandSpec, FakeCommandRunner,
        TokioCommandRunner,
    },
    infra::fs_paths::{
        safe_create_new_file_under_base, safe_directory_exists_under_base, safe_ensure_dir_path,
        safe_ensure_dir_under_base, safe_open_existing_file_path,
        safe_open_existing_file_under_base, safe_open_optional_existing_file_path,
        safe_open_optional_existing_file_under_base, safe_overwrite_file_path,
        safe_overwrite_file_under_base, safe_remove_dir_all_under_base,
        safe_remove_file_under_base, safe_rename_dir_under_base, safe_rename_file_under_base,
        safe_resolve_under_base,
    },
    validation::{
        ValidationError, validate_backup_archive_name, validate_cluster_name, validate_filename,
        validate_ku_id, validate_level_name, validate_mod_id, validate_safe_command_arg,
    },
};
use tempfile::tempdir;

#[cfg(windows)]
#[test]
fn windows_safe_filesystem_supports_create_write_move_and_delete() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("base");
    fs::create_dir(&base).unwrap();

    safe_ensure_dir_under_base(&base, "Cluster1/Master").unwrap();
    assert!(safe_directory_exists_under_base(&base, r"Cluster1\Master").unwrap());
    assert!(!safe_directory_exists_under_base(&base, "Missing").unwrap());

    let mut marker = safe_create_new_file_under_base(&base, "first.lock").unwrap();
    marker.write_all(b"locked").unwrap();
    drop(marker);
    assert!(safe_create_new_file_under_base(&base, "first.lock").is_err());
    assert!(safe_remove_file_under_base(&base, "first.lock").unwrap());
    assert!(!safe_remove_file_under_base(&base, "first.lock").unwrap());

    safe_overwrite_file_under_base(&base, r"Cluster1\Master\server.ini", b"first").unwrap();
    safe_rename_file_under_base(
        &base,
        "Cluster1/Master/server.ini",
        "Cluster1/Master/renamed.ini",
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(base.join("Cluster1/Master/renamed.ini")).unwrap(),
        "first"
    );
    fs::write(base.join("Cluster1/Master/existing.ini"), "keep").unwrap();
    assert!(
        safe_rename_file_under_base(
            &base,
            "Cluster1/Master/renamed.ini",
            "Cluster1/Master/existing.ini",
        )
        .is_err()
    );
    assert_eq!(
        fs::read_to_string(base.join("Cluster1/Master/existing.ini")).unwrap(),
        "keep"
    );

    safe_rename_dir_under_base(&base, "Cluster1", "Cluster2").unwrap();
    assert!(safe_directory_exists_under_base(&base, "Cluster2/Master").unwrap());
    safe_remove_dir_all_under_base(&base, "Cluster2").unwrap();
    assert!(!safe_directory_exists_under_base(&base, "Cluster2").unwrap());

    let external = dir.path().join("configured").join("nested");
    safe_ensure_dir_path(&external).unwrap();
    let file = external.join("settings.ini");
    safe_overwrite_file_path(&file, b"value").unwrap();
    assert!(safe_open_existing_file_path(&file).is_ok());
    assert_eq!(fs::read_to_string(file).unwrap(), "value");
}

#[cfg(windows)]
#[test]
fn windows_safe_filesystem_rejects_junction_escape() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("base");
    let outside = dir.path().join("outside");
    fs::create_dir(&base).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret.ini"), "keep").unwrap();

    let junction = base.join("escape");
    let result = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&outside)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "junction creation failed: {result:?}"
    );
    assert!(safe_directory_exists_under_base(&base, "escape").is_err());
    assert!(safe_ensure_dir_under_base(&base, "escape/new").is_err());
    assert!(safe_overwrite_file_under_base(&base, "escape/secret.ini", b"bad").is_err());
    assert!(safe_remove_dir_all_under_base(&base, "escape").is_err());
    assert_eq!(
        fs::read_to_string(outside.join("secret.ini")).unwrap(),
        "keep"
    );
}

#[cfg(windows)]
#[test]
fn windows_process_snapshots_include_the_running_panel_test() {
    let snapshots = SystemProcessSnapshotProvider.snapshots().unwrap();
    assert!(
        snapshots
            .iter()
            .any(|snapshot| snapshot.pid == Some(std::process::id()))
    );
}

#[cfg(windows)]
#[test]
fn windows_safe_open_serves_regular_files_and_rejects_unsafe_paths() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("dist");
    fs::create_dir_all(base.join("assets")).unwrap();
    fs::write(base.join("index.html"), "DST Admin").unwrap();
    fs::write(dir.path().join("secret.txt"), "outside").unwrap();

    let mut file = safe_open_existing_file_under_base(&base, "index.html").unwrap();
    let mut contents = String::new();
    file.read_to_string(&mut contents).unwrap();
    assert_eq!(contents, "DST Admin");
    assert!(
        safe_open_optional_existing_file_under_base(&base, "missing.html")
            .unwrap()
            .is_none()
    );
    assert!(safe_open_existing_file_under_base(&base, "assets").is_err());
    assert!(safe_open_existing_file_under_base(&base, "../secret.txt").is_err());

    // Creating symbolic links can require Developer Mode on Windows. When it
    // succeeds, both ancestor and leaf links must be rejected.
    if std::os::windows::fs::symlink_file(
        dir.path().join("secret.txt"),
        base.join("secret-link.txt"),
    )
    .is_ok()
    {
        assert!(safe_open_existing_file_under_base(&base, "secret-link.txt").is_err());
    }
    if std::os::windows::fs::symlink_dir(dir.path(), base.join("outside-link")).is_ok() {
        assert!(safe_open_existing_file_under_base(&base, "outside-link/secret.txt").is_err());
    }
}

#[test]
fn validators_accept_known_safe_values() {
    assert_eq!(
        validate_cluster_name("MasterShard").unwrap().as_str(),
        "MasterShard"
    );
    assert_eq!(validate_level_name("Caves-1").unwrap().as_str(), "Caves-1");
    assert_eq!(validate_mod_id("378160973").unwrap().as_str(), "378160973");
    assert_eq!(
        validate_ku_id("KU_abc-123_DEF").unwrap().as_str(),
        "KU_abc-123_DEF"
    );
    assert_eq!(
        validate_filename("cluster.ini").unwrap().as_str(),
        "cluster.ini"
    );
    assert_eq!(
        validate_backup_archive_name("backup-2026-06-11.zip")
            .unwrap()
            .as_str(),
        "backup-2026-06-11.zip"
    );
    assert_eq!(
        validate_safe_command_arg("cluster name", "MasterShard")
            .unwrap()
            .as_str(),
        "MasterShard"
    );
}

#[test]
fn validators_reject_unsafe_values_with_client_safe_errors() {
    assert_rejected(validate_cluster_name(""), "");
    assert_rejected(validate_level_name(".."), "..");
    assert_rejected(
        validate_filename("cluster/secret-token.ini"),
        "secret-token",
    );
    assert_rejected(
        validate_backup_archive_name("secret-token\u{7}.zip"),
        "secret-token",
    );
    assert_rejected(validate_mod_id("123abc"), "123abc");
    assert_rejected(validate_mod_id(&"1".repeat(21)), "");
    assert_rejected(validate_ku_id("KU_"), "KU_");
    assert_rejected(validate_ku_id("KU_../../secret-token"), "secret-token");
    assert_rejected(validate_ku_id(&format!("KU_{}", "A".repeat(129))), "");
    for value in [
        "bad:name",
        "bad<name",
        "bad>name",
        "bad\"name",
        "bad|name",
        "bad?name",
        "bad*name",
    ] {
        assert_rejected(validate_filename(value), value);
        assert_rejected(validate_cluster_name(value), value);
        assert_rejected(validate_backup_archive_name(value), value);
    }
    assert_rejected(
        validate_safe_command_arg("cluster name", "-cluster_token"),
        "-cluster_token",
    );
    assert_rejected(
        validate_safe_command_arg("cluster name", "+login"),
        "+login",
    );
    for value in [
        "Cluster;touch-pwned",
        "Cluster&touch-pwned",
        "Cluster`touch-pwned`",
        "Cluster$HOME",
        "Cluster(name)",
        "Cluster'name",
    ] {
        assert_rejected(validate_safe_command_arg("cluster name", value), value);
    }
}

#[test]
fn safe_resolver_accepts_existing_paths_and_future_leaf_under_base() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("cluster");
    fs::create_dir(&base).unwrap();
    fs::create_dir(base.join("Master")).unwrap();
    fs::write(
        base.join("Master").join("server.ini"),
        "server_port = 10999",
    )
    .unwrap();

    let existing = safe_resolve_under_base(&base, "Master/server.ini").unwrap();
    assert_eq!(
        existing,
        base.join("Master")
            .join("server.ini")
            .canonicalize()
            .unwrap()
    );

    let future_leaf = safe_resolve_under_base(&base, "Master/modoverrides.lua").unwrap();
    assert_eq!(
        future_leaf,
        base.canonicalize()
            .unwrap()
            .join("Master")
            .join("modoverrides.lua")
    );
}

#[test]
fn safe_resolver_rejects_absolute_traversal_and_illegal_components() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    fs::create_dir(base.join("Master")).unwrap();

    assert!(safe_resolve_under_base(base, "/tmp/secret-token").is_err());
    assert!(safe_resolve_under_base(base, "../secret-token").is_err());
    assert!(safe_resolve_under_base(base, "Master/../secret-token").is_err());
    assert!(safe_resolve_under_base(base, "Master/./server.ini").is_err());
    assert!(safe_resolve_under_base(base, "Master/\u{7}secret-token").is_err());
    #[cfg(not(windows))]
    {
        assert_backslash_path_rejected(base, r"Master\server.ini");
        assert_backslash_path_rejected(base, r"Master\.\server.ini");
        assert_backslash_path_rejected(base, r"Master\\server.ini");
    }
    #[cfg(windows)]
    {
        assert!(safe_resolve_under_base(base, r"Master\server.ini").is_ok());
        assert!(safe_resolve_under_base(base, r"Master\.\server.ini").is_err());
        assert!(safe_resolve_under_base(base, r"Master\\server.ini").is_err());
        assert!(safe_resolve_under_base(base, r"Master\..\server.ini").is_err());
    }
}

#[cfg(not(windows))]
fn assert_backslash_path_rejected(base: &std::path::Path, path: &str) {
    let error = safe_resolve_under_base(base, path).unwrap_err();
    assert!(
        error.to_string().contains("backslash"),
        "{path} should be rejected before platform-specific path parsing: {error}"
    );
}

#[cfg(unix)]
#[test]
fn safe_remove_file_under_base_removes_regular_files_and_rejects_symlinks() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let base = dir.path().join("base");
    let outside = dir.path().join("outside");
    fs::create_dir(&base).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::create_dir(base.join("Master")).unwrap();
    fs::write(base.join("Master/server_log.txt"), "delete").unwrap();
    fs::write(outside.join("keep.txt"), "keep").unwrap();
    symlink(&outside, base.join("escape")).unwrap();
    symlink(outside.join("keep.txt"), base.join("leaf-link.txt")).unwrap();

    assert!(safe_remove_file_under_base(&base, "Master/server_log.txt").unwrap());
    assert!(!base.join("Master/server_log.txt").exists());
    assert!(!safe_remove_file_under_base(&base, "Master/missing.txt").unwrap());
    assert!(safe_remove_file_under_base(&base, "escape/keep.txt").is_err());
    assert!(safe_remove_file_under_base(&base, "leaf-link.txt").is_err());
    assert_eq!(
        fs::read_to_string(outside.join("keep.txt")).unwrap(),
        "keep"
    );
}

#[cfg(unix)]
#[test]
fn safe_resolver_rejects_symlink_escapes_for_existing_parent_and_leaf() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let base = dir.path().join("base");
    let outside = dir.path().join("outside");
    fs::create_dir(&base).unwrap();
    fs::create_dir(&outside).unwrap();
    symlink(&outside, base.join("escape")).unwrap();
    fs::write(outside.join("secret-token.txt"), "do-not-open").unwrap();
    symlink(
        outside.join("secret-token.txt"),
        base.join("secret-link.txt"),
    )
    .unwrap();

    let err = safe_resolve_under_base(&base, "escape/new-file.txt").unwrap_err();
    assert!(!err.to_string().contains("outside"));

    let err = safe_resolve_under_base(&base, "secret-link.txt").unwrap_err();
    assert!(!err.to_string().contains("secret-token"));
}

#[test]
fn safe_create_new_file_under_base_rejects_existing_leaf_and_writes_future_leaf() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("base");
    fs::create_dir(&base).unwrap();
    fs::create_dir(base.join("Master")).unwrap();

    let mut created = safe_create_new_file_under_base(&base, "Master/cluster.ini").unwrap();
    created.write_all(b"[GAMEPLAY]").unwrap();
    drop(created);

    assert_eq!(
        fs::read_to_string(base.join("Master").join("cluster.ini")).unwrap(),
        "[GAMEPLAY]"
    );
    assert!(
        safe_create_new_file_under_base(&base, "Master/cluster.ini").is_err(),
        "safe create must not follow or overwrite an existing leaf"
    );
}

#[cfg(unix)]
#[test]
fn safe_create_new_file_under_base_rejects_symlink_ancestor_and_leaf() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let base = dir.path().join("base");
    let outside = dir.path().join("outside");
    fs::create_dir(&base).unwrap();
    fs::create_dir(&outside).unwrap();
    symlink(&outside, base.join("escape")).unwrap();
    symlink(outside.join("created.txt"), base.join("leaf-link.txt")).unwrap();

    assert!(safe_create_new_file_under_base(&base, "escape/created.txt").is_err());
    assert!(safe_create_new_file_under_base(&base, "leaf-link.txt").is_err());
    assert!(!outside.join("created.txt").exists());
}

#[test]
fn safe_ensure_dir_under_base_creates_nested_directories() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("base");
    fs::create_dir(&base).unwrap();

    safe_ensure_dir_under_base(&base, "Cluster1/Master").unwrap();

    assert!(base.join("Cluster1").join("Master").is_dir());
}

#[cfg(unix)]
#[test]
fn safe_ensure_dir_under_base_rejects_symlink_ancestor_without_creating_outside() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let base = dir.path().join("base");
    let outside = dir.path().join("outside");
    fs::create_dir(&base).unwrap();
    fs::create_dir(&outside).unwrap();
    symlink(&outside, base.join("ClusterLink")).unwrap();

    assert!(safe_ensure_dir_under_base(&base, "ClusterLink/Master").is_err());
    assert!(!outside.join("Master").exists());
}

#[cfg(unix)]
#[test]
fn safe_ensure_dir_path_rejects_existing_path_with_symlink_ancestor() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let base = dir.path().join("base");
    let outside = dir.path().join("outside");
    fs::create_dir(&base).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::create_dir(outside.join("existing")).unwrap();
    symlink(&outside, base.join("link")).unwrap();

    assert!(safe_ensure_dir_path(base.join("link/existing")).is_err());
}

#[cfg(unix)]
#[test]
fn safe_ensure_dir_path_rejects_dotdot_after_symlink_component() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let base = dir.path().join("base");
    let outside_parent = dir.path().join("outside-parent");
    let outside = outside_parent.join("outside");
    fs::create_dir(&base).unwrap();
    fs::create_dir(&outside_parent).unwrap();
    fs::create_dir(&outside).unwrap();
    symlink(&outside, base.join("link")).unwrap();

    assert!(safe_ensure_dir_path(base.join("link/../escaped")).is_err());
    assert!(!outside_parent.join("escaped").exists());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn safe_rename_dir_under_base_refuses_existing_destination_without_replacing_it() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("base");
    fs::create_dir(&base).unwrap();
    fs::create_dir(base.join("source")).unwrap();
    fs::write(base.join("source/keep.txt"), "source").unwrap();
    fs::create_dir(base.join("destination")).unwrap();
    fs::write(base.join("destination/keep.txt"), "destination").unwrap();

    assert!(safe_rename_dir_under_base(&base, "source", "destination").is_err());
    assert_eq!(
        fs::read_to_string(base.join("source/keep.txt")).unwrap(),
        "source"
    );
    assert_eq!(
        fs::read_to_string(base.join("destination/keep.txt")).unwrap(),
        "destination"
    );
}

#[test]
fn safe_overwrite_file_under_base_writes_existing_and_new_regular_files() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("base");
    fs::create_dir(&base).unwrap();
    fs::create_dir(base.join("Master")).unwrap();
    fs::write(base.join("Master").join("modoverrides.lua"), "return {}").unwrap();

    safe_overwrite_file_under_base(
        &base,
        "Master/modoverrides.lua",
        "return { enabled = true }",
    )
    .unwrap();
    safe_overwrite_file_under_base(&base, "Master/server.ini", "server_port = 10999").unwrap();

    assert_eq!(
        fs::read_to_string(base.join("Master").join("modoverrides.lua")).unwrap(),
        "return { enabled = true }"
    );
    assert_eq!(
        fs::read_to_string(base.join("Master").join("server.ini")).unwrap(),
        "server_port = 10999"
    );
}

#[cfg(unix)]
#[test]
fn safe_overwrite_file_under_base_rejects_symlink_ancestor_and_leaf() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let base = dir.path().join("base");
    let outside = dir.path().join("outside");
    fs::create_dir(&base).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("modoverrides.lua"), "return { keep = true }").unwrap();
    symlink(&outside, base.join("escape")).unwrap();
    symlink(
        outside.join("modoverrides.lua"),
        base.join("modoverrides-link.lua"),
    )
    .unwrap();

    assert!(safe_overwrite_file_under_base(&base, "escape/modoverrides.lua", "bad").is_err());
    assert!(safe_overwrite_file_under_base(&base, "modoverrides-link.lua", "bad").is_err());
    assert_eq!(
        fs::read_to_string(outside.join("modoverrides.lua")).unwrap(),
        "return { keep = true }"
    );
}

#[cfg(unix)]
#[test]
fn safe_open_existing_file_under_base_rejects_symlink_ancestor_and_leaf() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let base = dir.path().join("base");
    let outside = dir.path().join("outside");
    fs::create_dir(&base).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("server.ini"), "do-not-open").unwrap();
    symlink(&outside, base.join("escape")).unwrap();
    symlink(outside.join("server.ini"), base.join("server-link.ini")).unwrap();

    assert!(safe_open_existing_file_under_base(&base, "escape/server.ini").is_err());
    assert!(safe_open_existing_file_under_base(&base, "server-link.ini").is_err());
}

#[cfg(unix)]
#[test]
fn safe_open_optional_existing_file_under_base_rejects_symlink_parent_even_for_missing_leaf() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let base = dir.path().join("base");
    let outside = dir.path().join("outside");
    fs::create_dir(&base).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::create_dir(base.join("Master")).unwrap();
    symlink(&outside, base.join("LinkedMaster")).unwrap();

    assert!(
        safe_open_optional_existing_file_under_base(&base, "Master/missing.lua")
            .unwrap()
            .is_none()
    );
    assert!(
        safe_open_optional_existing_file_under_base(&base, "LinkedMaster/missing.lua").is_err()
    );
}

#[test]
fn safe_open_optional_existing_file_under_base_treats_missing_parent_as_absent_leaf() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("base");
    fs::create_dir(&base).unwrap();

    let file = safe_open_optional_existing_file_under_base(&base, "MissingParent/missing.lua")
        .expect("missing parent under a trusted base is an absent optional file");

    assert!(file.is_none());
}

#[cfg(unix)]
#[test]
fn safe_open_existing_file_under_base_rejects_fifo_leaf_without_waiting_for_writer() {
    use std::{
        ffi::CString,
        os::{unix::ffi::OsStrExt, unix::fs::OpenOptionsExt},
        thread,
    };

    let dir = tempdir().unwrap();
    let base = dir.path().join("base");
    fs::create_dir(&base).unwrap();
    let fifo = base.join("server_log.txt");
    let fifo_c_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    let mkfifo_result = unsafe { libc::mkfifo(fifo_c_path.as_ptr(), 0o644) };
    assert_eq!(mkfifo_result, 0);

    let writer_fifo = fifo.clone();
    let writer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(250));
        let mut options = fs::OpenOptions::new();
        options.write(true).custom_flags(libc::O_NONBLOCK);
        let _ = options.open(writer_fifo);
    });

    let started = Instant::now();
    let error = safe_open_existing_file_under_base(&base, "server_log.txt").unwrap_err();
    let elapsed = started.elapsed();
    writer.join().unwrap();

    assert!(
        elapsed < Duration::from_millis(100),
        "safe open should reject FIFO leaves before a writer appears; elapsed={elapsed:?}"
    );
    assert!(error.to_string().contains("not a file"));
}

#[cfg(unix)]
#[test]
fn safe_absolute_open_rejects_symlink_escape_and_normalized_components() {
    use std::{io::Read, os::unix::fs::symlink};

    let dir = tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let base = root.join("base");
    let outside = root.join("outside");
    fs::create_dir(&base).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::create_dir(base.join("nested")).unwrap();
    fs::write(base.join("config.txt"), "inside").unwrap();
    fs::write(outside.join("secret.txt"), "outside").unwrap();
    symlink(&outside, base.join("escape")).unwrap();
    symlink(outside.join("secret.txt"), base.join("leaf-link.txt")).unwrap();

    let mut opened = safe_open_existing_file_path(base.join("config.txt")).unwrap();
    let mut contents = String::new();
    opened.read_to_string(&mut contents).unwrap();
    assert_eq!(contents, "inside");

    assert!(safe_open_existing_file_path(base.join("escape/secret.txt")).is_err());
    assert!(safe_open_existing_file_path(base.join("leaf-link.txt")).is_err());
    assert!(
        safe_open_existing_file_path(format!("{}/nested/../config.txt", base.display())).is_err()
    );
    assert!(safe_open_existing_file_path(format!("{}/./config.txt", base.display())).is_err());
    assert!(
        safe_open_existing_file_path(format!("{}/nested//config.txt", base.display())).is_err()
    );
}

#[cfg(unix)]
#[test]
fn safe_absolute_optional_open_distinguishes_missing_from_unsafe_paths() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let base = root.join("base");
    let outside = root.join("outside");
    fs::create_dir(&base).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret.txt"), "outside").unwrap();
    symlink(&outside, base.join("escape")).unwrap();
    symlink(outside.join("secret.txt"), base.join("leaf-link.txt")).unwrap();

    assert!(
        safe_open_optional_existing_file_path(base.join("missing/file.txt"))
            .unwrap()
            .is_none()
    );
    assert!(!base.join("missing").exists());
    assert!(safe_open_optional_existing_file_path(base.join("escape/secret.txt")).is_err());
    assert!(safe_open_optional_existing_file_path(base.join("leaf-link.txt")).is_err());
}

#[cfg(unix)]
#[test]
fn safe_absolute_overwrite_rejects_symlink_escape_and_leaf() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let base = root.join("base");
    let outside = root.join("outside");
    fs::create_dir(&base).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(base.join("config.txt"), "old").unwrap();
    fs::write(outside.join("secret.txt"), "outside").unwrap();
    symlink(&outside, base.join("escape")).unwrap();
    symlink(outside.join("secret.txt"), base.join("leaf-link.txt")).unwrap();

    safe_overwrite_file_path(base.join("config.txt"), b"new").unwrap();

    assert_eq!(fs::read_to_string(base.join("config.txt")).unwrap(), "new");
    assert!(safe_overwrite_file_path(base.join("escape/secret.txt"), b"pwned").is_err());
    assert!(safe_overwrite_file_path(base.join("leaf-link.txt"), b"pwned").is_err());
    assert_eq!(
        fs::read_to_string(outside.join("secret.txt")).unwrap(),
        "outside"
    );
}

#[tokio::test]
async fn fake_command_runner_records_argv_and_returns_configured_output() {
    let runner = FakeCommandRunner::new(vec![CommandOutput::success(
        b"updated".to_vec(),
        Vec::new(),
    )]);
    let spec = CommandSpec::new("steamcmd")
        .arg("+login")
        .arg("anonymous")
        .arg("+quit");

    let output = runner.run(spec).await.unwrap();

    assert_eq!(output.status_code, Some(0));
    assert_eq!(output.stdout, b"updated");
    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].program(), "steamcmd");
    assert_eq!(calls[0].args(), ["+login", "anonymous", "+quit"]);
}

#[test]
fn command_spec_debug_redacts_sensitive_argument_values() {
    let spec = CommandSpec::new("dst-server")
        .arg("-cluster_token")
        .arg("secret-token")
        .arg("--session")
        .arg("secret-session");

    let debug = format!("{spec:?}");

    assert!(debug.contains("dst-server"));
    assert!(debug.contains("args_len"));
    assert!(!debug.contains("secret-token"));
    assert!(!debug.contains("secret-session"));
}

#[test]
fn command_output_debug_redacts_captured_stdout_and_stderr() {
    let output = CommandOutput {
        status_code: Some(0),
        stdout: b"secret-token".to_vec(),
        stderr: b"secret-session".to_vec(),
        timed_out: false,
        stdout_truncated: true,
        stderr_truncated: true,
    };

    let debug = format!("{output:?}");

    assert!(debug.contains("stdout_len"));
    assert!(debug.contains("stderr_len"));
    assert!(!debug.contains("stdout: ["));
    assert!(!debug.contains("stderr: ["));
    assert!(!debug.contains("secret-token"));
    assert!(!debug.contains("secret-session"));
}

#[test]
fn tokio_command_runner_exposes_process_tree_cleanup_support() {
    assert_eq!(
        TokioCommandRunner::process_tree_cleanup_supported(),
        cfg!(unix) || cfg!(windows),
        "external command writes must fail closed without process-tree timeout cleanup"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn windows_command_runner_executes_and_times_out() {
    let runner = TokioCommandRunner::new();
    let output = runner
        .run(CommandSpec::new("cmd.exe").extend_args(["/C", "echo ready"]))
        .await
        .unwrap();
    assert_eq!(output.status_code, Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("ready"));

    let start = Instant::now();
    let error = runner
        .run(
            CommandSpec::new("powershell.exe")
                .extend_args(["-NoProfile", "-Command", "Start-Sleep -Seconds 5"])
                .with_timeout(Duration::from_millis(300)),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, CommandError::Timeout));
    assert!(start.elapsed() < Duration::from_secs(3));
}

#[cfg(windows)]
#[tokio::test]
async fn windows_screen_adapter_launches_and_writes_console_input() {
    let dir = tempdir().unwrap();
    let marker = dir.path().join("console-command.txt");
    let escaped = marker.display().to_string().replace('\'', "''");
    let script =
        format!("$line = [Console]::ReadLine(); [IO.File]::WriteAllText('{escaped}', $line)");
    let powershell = std::path::PathBuf::from(
        std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_owned()),
    )
    .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let session = "DST_8level_WindowsTest_Master";
    let runner = TokioCommandRunner::new();
    let launch = CommandSpec::new("screen")
        .extend_args(["-d", "-m", "-S", session])
        .arg(powershell.display().to_string())
        .extend_args(["-NoProfile", "-Command"])
        .arg(script)
        .with_current_dir(dir.path());
    assert_eq!(runner.run(launch).await.unwrap().status_code, Some(0));

    let send = CommandSpec::new("screen")
        .extend_args(["-S", session, "-p", "0", "-X", "stuff", "hello\n"]);
    assert_eq!(runner.run(send).await.unwrap().status_code, Some(0));
    for _ in 0..50 {
        if marker.exists() {
            assert_eq!(fs::read_to_string(marker).unwrap(), "hello");
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("Windows console command did not reach the launched process");
}

#[cfg(unix)]
#[tokio::test]
async fn tokio_command_runner_truncates_large_stdout_without_using_shell_strings() {
    let runner = TokioCommandRunner::new();
    let output = runner
        .run(
            CommandSpec::new("/usr/bin/printf")
                .arg("abcdefghijklmnopqrstuvwxyz")
                .with_output_limit(8),
        )
        .await
        .unwrap();

    assert_eq!(output.status_code, Some(0));
    assert_eq!(output.stdout, b"abcdefgh");
    assert!(output.stdout_truncated);
    assert!(output.stderr.is_empty());
    assert!(!output.stderr_truncated);
}

#[cfg(unix)]
#[tokio::test]
async fn tokio_command_runner_times_out_long_running_processes() {
    let runner = TokioCommandRunner::new();
    let started = Instant::now();
    let error = runner
        .run(
            CommandSpec::new("/bin/sh")
                .extend_args(["-c", "sleep 5"])
                .with_timeout(Duration::from_millis(50)),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, CommandError::Timeout));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "timeout should kill and reap the process promptly"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn tokio_command_runner_timeout_kills_descendant_process_group_members() {
    let dir = tempdir().unwrap();
    let marker = dir.path().join("descendant-survived");
    let runner = TokioCommandRunner::new();
    let script = format!("(sleep 1; touch '{}') & wait", marker.display());

    let error = runner
        .run(
            CommandSpec::new("/bin/sh")
                .extend_args(["-c", script.as_str()])
                .with_timeout(Duration::from_millis(50)),
        )
        .await
        .unwrap_err();
    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert!(matches!(error, CommandError::Timeout));
    assert!(
        !marker.exists(),
        "timeout should kill descendants in the command process group"
    );
}

fn assert_rejected<T>(result: Result<T, ValidationError>, raw_fragment: &str) {
    let error = match result {
        Ok(_) => panic!("expected validation to reject unsafe value"),
        Err(error) => error,
    };
    let msg = error.to_string();

    assert!(msg.starts_with("invalid "));
    if !raw_fragment.is_empty() {
        assert!(!msg.contains(raw_fragment));
    }
}
