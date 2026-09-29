//! Handle-anchored Windows implementations for safe filesystem operations.

use std::{
    fs::File,
    io::{self, Write},
    os::windows::{ffi::OsStrExt, io::AsRawHandle},
    path::{Path, PathBuf},
};

use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};

use super::{
    FsPathError, safe_directory_path_exists_inner, safe_leaf, safe_parent,
    safe_relative_components, validate_absolute_path, windows_final_path_by_raw_handle,
};

fn open_base(base: &Path) -> Result<Dir, FsPathError> {
    let base = base
        .canonicalize()
        .map_err(|_| FsPathError::new("base directory is unavailable"))?;
    Dir::open_ambient_dir(base, ambient_authority())
        .map_err(|_| FsPathError::new("base directory is unavailable"))
}

fn checked_child_dir(parent: &Dir, component: &str) -> Result<Option<Dir>, FsPathError> {
    let metadata = match parent.symlink_metadata(component) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(FsPathError::new("directory path is unavailable")),
    };
    if metadata.is_symlink() {
        return Err(FsPathError::new("directory path contains a reparse point"));
    }
    if !metadata.is_dir() {
        return Err(FsPathError::new("path is not a directory"));
    }
    parent
        .open_dir(component)
        .map(Some)
        .map_err(|_| FsPathError::new("directory path is unavailable"))
}

fn walk_existing(base: &Path, components: &[String]) -> Result<Option<Dir>, FsPathError> {
    let mut dir = open_base(base)?;
    for component in components {
        let Some(next) = checked_child_dir(&dir, component)? else {
            return Ok(None);
        };
        dir = next;
    }
    Ok(Some(dir))
}

fn parent_and_leaf(base: &Path, path: &Path) -> Result<Option<(Dir, String)>, FsPathError> {
    let components = safe_relative_components(path)?;
    let (leaf, ancestors) = components
        .split_last()
        .ok_or_else(|| FsPathError::new("path cannot be empty"))?;
    Ok(walk_existing(base, ancestors)?.map(|parent| (parent, leaf.clone())))
}

fn checked_leaf_file(parent: &Dir, leaf: &str) -> Result<bool, FsPathError> {
    match parent.symlink_metadata(leaf) {
        Ok(metadata) if metadata.is_symlink() => {
            Err(FsPathError::new("path contains a reparse point"))
        }
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(FsPathError::new("path is not a regular file")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(FsPathError::new("file is unavailable")),
    }
}

pub(super) fn safe_directory_exists_under_base(
    base: &Path,
    path: &Path,
) -> Result<bool, FsPathError> {
    let components = safe_relative_components(path)?;
    walk_existing(base, &components).map(|dir| dir.is_some())
}

pub(super) fn safe_ensure_dir_under_base(base: &Path, path: &Path) -> Result<(), FsPathError> {
    let components = safe_relative_components(path)?;
    let mut dir = open_base(base)?;
    for component in components {
        if checked_child_dir(&dir, &component)?.is_none() {
            match dir.create_dir(&component) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(FsPathError::new("directory could not be created safely")),
            }
        }
        dir = checked_child_dir(&dir, &component)?
            .ok_or_else(|| FsPathError::new("directory is unavailable after creation"))?;
    }
    Ok(())
}

pub(super) fn safe_ensure_dir_path(path: &Path) -> Result<(), FsPathError> {
    validate_absolute_path(path)?;
    if safe_directory_path_exists_inner(path)? {
        return Ok(());
    }
    let parent = safe_parent(path)?;
    safe_ensure_dir_path(parent)?;
    safe_ensure_dir_under_base(parent, Path::new(safe_leaf(path)?))
}

pub(super) fn safe_open_existing_file_path(path: &Path) -> Result<File, FsPathError> {
    safe_open_optional_existing_file_path(path)?
        .ok_or_else(|| FsPathError::new("file is unavailable"))
}

pub(super) fn safe_open_optional_existing_file_path(
    path: &Path,
) -> Result<Option<File>, FsPathError> {
    validate_absolute_path(path)?;
    let parent = safe_parent(path)?;
    if !safe_directory_path_exists_inner(parent)? {
        return Ok(None);
    }
    super::safe_open_optional_existing_file_under_base(parent, Path::new(safe_leaf(path)?))
}

pub(super) fn safe_create_new_file_under_base(
    base: &Path,
    path: &Path,
) -> Result<File, FsPathError> {
    let (parent, leaf) = parent_and_leaf(base, path)?
        .ok_or_else(|| FsPathError::new("parent directory is unavailable"))?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    parent
        .open_with(&leaf, &options)
        .map(|file| file.into_std())
        .map_err(|_| FsPathError::new("file could not be created safely"))
}

pub(super) fn safe_overwrite_file_under_base(
    base: &Path,
    path: &Path,
    contents: &[u8],
) -> Result<(), FsPathError> {
    let (parent, leaf) = parent_and_leaf(base, path)?
        .ok_or_else(|| FsPathError::new("parent directory is unavailable"))?;
    checked_leaf_file(&parent, &leaf)?;
    let mut options = OpenOptions::new();
    options.write(true).create(true);
    let mut file = parent
        .open_with(&leaf, &options)
        .map_err(|_| FsPathError::new("file could not be opened safely"))?;
    if !file
        .metadata()
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
    {
        return Err(FsPathError::new("path is not a regular file"));
    }
    file.set_len(0)
        .map_err(|_| FsPathError::new("file could not be truncated"))?;
    file.write_all(contents)
        .map_err(|_| FsPathError::new("file could not be written"))?;
    file.flush()
        .map_err(|_| FsPathError::new("file could not be flushed"))
}

pub(super) fn safe_overwrite_file_path(path: &Path, contents: &[u8]) -> Result<(), FsPathError> {
    validate_absolute_path(path)?;
    let parent = safe_parent(path)?;
    if !safe_directory_path_exists_inner(parent)? {
        return Err(FsPathError::new("parent directory is unavailable"));
    }
    safe_overwrite_file_under_base(parent, Path::new(safe_leaf(path)?), contents)
}

pub(super) fn safe_remove_file_under_base(base: &Path, path: &Path) -> Result<bool, FsPathError> {
    let Some((parent, leaf)) = parent_and_leaf(base, path)? else {
        return Ok(false);
    };
    if !checked_leaf_file(&parent, &leaf)? {
        return Ok(false);
    }
    parent
        .remove_file(&leaf)
        .map_err(|_| FsPathError::new("file could not be deleted safely"))?;
    Ok(true)
}

pub(super) fn safe_remove_dir_all_under_base(base: &Path, path: &Path) -> Result<(), FsPathError> {
    let (parent, leaf) = parent_and_leaf(base, path)?
        .ok_or_else(|| FsPathError::new("parent directory is unavailable"))?;
    let dir = checked_child_dir(&parent, &leaf)?
        .ok_or_else(|| FsPathError::new("directory is unavailable"))?;
    dir.remove_open_dir_all()
        .map_err(|_| FsPathError::new("directory could not be deleted safely"))
}

pub(super) fn safe_rename_dir_under_base(
    base: &Path,
    from: &Path,
    to: &Path,
) -> Result<(), FsPathError> {
    rename_no_replace(base, from, to, true)
}

pub(super) fn safe_rename_file_under_base(
    base: &Path,
    from: &Path,
    to: &Path,
) -> Result<(), FsPathError> {
    rename_no_replace(base, from, to, false)
}

fn rename_no_replace(
    base: &Path,
    from: &Path,
    to: &Path,
    directory: bool,
) -> Result<(), FsPathError> {
    let (source_parent, source_leaf) = parent_and_leaf(base, from)?
        .ok_or_else(|| FsPathError::new("source parent directory is unavailable"))?;
    let (target_parent, target_leaf) = parent_and_leaf(base, to)?
        .ok_or_else(|| FsPathError::new("target parent directory is unavailable"))?;
    let source = source_parent
        .symlink_metadata(&source_leaf)
        .map_err(|_| FsPathError::new("source path is unavailable"))?;
    if source.is_symlink() || (directory && !source.is_dir()) || (!directory && !source.is_file()) {
        return Err(FsPathError::new("source path has an unsafe type"));
    }
    match target_parent.symlink_metadata(&target_leaf) {
        Ok(_) => return Err(FsPathError::new("target path already exists")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(FsPathError::new("target path is unavailable")),
    }

    // MoveFileW never replaces an existing destination. Both parent directory
    // handles stay open without FILE_SHARE_DELETE while their final paths are
    // used, so their names cannot be redirected by a concurrent rename.
    let source_path =
        windows_final_path_by_raw_handle(source_parent.as_raw_handle())?.join(source_leaf);
    let target_path =
        windows_final_path_by_raw_handle(target_parent.as_raw_handle())?.join(target_leaf);
    move_file_no_replace(&source_path, &target_path)
}

fn move_file_no_replace(source: &Path, target: &Path) -> Result<(), FsPathError> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileW(existing: *const u16, new: *const u16) -> i32;
    }

    let source_wide: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target_wide: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both strings are NUL-terminated and remain live for the call.
    if unsafe { MoveFileW(source_wide.as_ptr(), target_wide.as_ptr()) } == 0 {
        return Err(FsPathError::new(format!(
            "path could not be renamed safely: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(())
}
