//! Safe filesystem path helpers for resolving user-controlled relative paths.

use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

#[cfg(unix)]
use std::{
    ffi::{CStr, CString},
    mem,
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::fs::OpenOptionsExt,
    },
};

use thiserror::Error;

#[cfg(windows)]
mod windows;

/// Error raised when a user path cannot be resolved safely under a base path.
#[derive(Debug, Clone, Error)]
#[error("invalid path: {reason}")]
pub struct FsPathError {
    reason: Arc<str>,
}

impl FsPathError {
    fn new(reason: impl Into<Arc<str>>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// Resolves a user-supplied relative path under `base` without opening it.
///
/// Existing paths are canonicalized and must remain under the canonical base.
/// If the final leaf does not exist, its existing parent is canonicalized first
/// so symlinked parents cannot redirect a future create outside `base`.
///
/// Callers that will create a new file should prefer
/// [`safe_create_new_file_under_base`] so the path check and file creation stay
/// coupled. A resolved future path is only a location decision, not an atomic
/// authorization for a later write.
pub fn safe_resolve_under_base(
    base: impl AsRef<Path>,
    user_path: impl AsRef<Path>,
) -> Result<PathBuf, FsPathError> {
    let base = base.as_ref();
    let user_path = user_path.as_ref();
    let base = base
        .canonicalize()
        .map_err(|_| FsPathError::new("base directory is unavailable"))?;
    let components = safe_relative_components(user_path)?;
    let candidate = components
        .iter()
        .fold(base.clone(), |path, component| path.join(component));

    if candidate.exists() {
        let canonical = candidate
            .canonicalize()
            .map_err(|_| FsPathError::new("path is unavailable"))?;
        if !canonical.starts_with(&base) {
            return Err(FsPathError::new("path escapes base directory"));
        }
        return Ok(canonical);
    }

    let parent = candidate
        .parent()
        .ok_or_else(|| FsPathError::new("missing parent directory"))?;
    let canonical_parent = parent
        .canonicalize()
        .map_err(|_| FsPathError::new("parent directory is unavailable"))?;
    if !canonical_parent.starts_with(&base) {
        return Err(FsPathError::new("path escapes base directory"));
    }

    let leaf = components
        .last()
        .ok_or_else(|| FsPathError::new("path cannot be empty"))?;
    Ok(canonical_parent.join(leaf))
}

/// Checks whether an absolute directory path exists without accepting symlinks.
///
/// This helper validates the target itself and walks upward only while
/// components are missing. For high-risk relative trees such as DST cluster
/// paths, prefer [`safe_directory_exists_under_base`], which checks each
/// relative component from a trusted base descriptor.
pub fn safe_directory_path_exists(path: impl AsRef<Path>) -> Result<bool, FsPathError> {
    let path = path.as_ref();
    validate_absolute_path(path)?;
    safe_directory_path_exists_inner(path)
}

/// Checks whether a directory exists under `base` without following symlink
/// components in `user_path`.
pub fn safe_directory_exists_under_base(
    base: impl AsRef<Path>,
    user_path: impl AsRef<Path>,
) -> Result<bool, FsPathError> {
    #[cfg(unix)]
    {
        safe_directory_exists_under_base_unix(base.as_ref(), user_path.as_ref())
    }

    #[cfg(windows)]
    {
        windows::safe_directory_exists_under_base(base.as_ref(), user_path.as_ref())
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = (base, user_path);
        Err(FsPathError::new(
            "safe directory lookup is unsupported on this platform",
        ))
    }
}

/// Ensures an absolute directory path exists without following symlink
/// ancestors in the missing suffix.
///
/// Missing components are created one level at a time through
/// [`safe_ensure_dir_under_base`]. Callers that have a known trusted base should
/// prefer `safe_ensure_dir_under_base(base, relative_path)`.
pub fn safe_ensure_dir_path(path: impl AsRef<Path>) -> Result<(), FsPathError> {
    #[cfg(unix)]
    {
        let path = path.as_ref();
        validate_absolute_path(path)?;
        if safe_directory_path_exists_inner(path)? {
            return Ok(());
        }
        let parent = safe_parent(path)?;
        safe_ensure_dir_path(parent)?;
        let leaf = safe_leaf(path)?;
        safe_ensure_dir_under_base(parent, leaf)
    }

    #[cfg(windows)]
    {
        windows::safe_ensure_dir_path(path.as_ref())
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = path;
        Err(FsPathError::new(
            "safe directory creation is unsupported on this platform",
        ))
    }
}

/// Opens an existing regular file under `base` from a user-supplied relative path.
///
/// On Unix this opens each ancestor with `openat` plus
/// `O_DIRECTORY | O_NOFOLLOW`, then opens the leaf with `O_NOFOLLOW` and
/// verifies the opened descriptor is a file. Windows verifies the final path
/// of the opened file handle remains beneath the canonical base directory.
/// Other platforms fail closed until a safe implementation is added.
pub fn safe_open_existing_file_under_base(
    base: impl AsRef<Path>,
    user_path: impl AsRef<Path>,
) -> Result<File, FsPathError> {
    #[cfg(unix)]
    {
        safe_open_existing_file_under_base_unix(base.as_ref(), user_path.as_ref())
    }

    #[cfg(windows)]
    {
        safe_open_existing_file_under_base_fallback(base.as_ref(), user_path.as_ref())
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        safe_open_existing_file_under_base_fallback(base.as_ref(), user_path.as_ref())
    }
}

/// Opens an existing regular file at an absolute path without following
/// symlink ancestors or the leaf.
pub fn safe_open_existing_file_path(path: impl AsRef<Path>) -> Result<File, FsPathError> {
    #[cfg(unix)]
    {
        safe_open_existing_file_path_unix(path.as_ref())
    }

    #[cfg(windows)]
    {
        windows::safe_open_existing_file_path(path.as_ref())
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = path;
        Err(FsPathError::new(
            "safe file opening is unsupported on this platform",
        ))
    }
}

/// Opens an optional regular file under `base`.
///
/// Missing leaves return `Ok(None)` only after every parent component has been
/// opened or validated without following symlinks. Symlink ancestors, symlink
/// leaves, non-file leaves, and unsafe paths are errors. Windows uses the same
/// handle-path containment check as [`safe_open_existing_file_under_base`].
/// Other platforms fail closed until a safe implementation is added.
pub fn safe_open_optional_existing_file_under_base(
    base: impl AsRef<Path>,
    user_path: impl AsRef<Path>,
) -> Result<Option<File>, FsPathError> {
    #[cfg(unix)]
    {
        safe_open_optional_existing_file_under_base_unix(base.as_ref(), user_path.as_ref())
    }

    #[cfg(windows)]
    {
        safe_open_optional_existing_file_under_base_fallback(base.as_ref(), user_path.as_ref())
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        safe_open_optional_existing_file_under_base_fallback(base.as_ref(), user_path.as_ref())
    }
}

/// Opens an optional regular file at an absolute path without following
/// symlink ancestors or the leaf.
pub fn safe_open_optional_existing_file_path(
    path: impl AsRef<Path>,
) -> Result<Option<File>, FsPathError> {
    #[cfg(unix)]
    {
        safe_open_optional_existing_file_path_unix(path.as_ref())
    }

    #[cfg(windows)]
    {
        windows::safe_open_optional_existing_file_path(path.as_ref())
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = path;
        Err(FsPathError::new(
            "safe optional file opening is unsupported on this platform",
        ))
    }
}

/// Atomically creates a new file under `base` from a user-supplied relative path.
///
/// The helper refuses existing leaves. On Unix it opens each ancestor with
/// `openat` plus `O_DIRECTORY | O_NOFOLLOW`, then creates the leaf with
/// `O_CREAT | O_EXCL | O_NOFOLLOW`; that keeps parent traversal tied to open
/// file descriptors instead of re-resolving a string path. Later overwrite
/// workflows should use a separate helper with the same no-follow discipline
/// instead of resolving first and writing later.
///
/// Windows uses a capability directory handle to keep traversal beneath the
/// trusted base. Other platforms fail closed until an equivalent safe create
/// implementation is added.
pub fn safe_create_new_file_under_base(
    base: impl AsRef<Path>,
    user_path: impl AsRef<Path>,
) -> Result<File, FsPathError> {
    #[cfg(unix)]
    {
        safe_create_new_file_under_base_unix(base.as_ref(), user_path.as_ref())
    }

    #[cfg(windows)]
    {
        windows::safe_create_new_file_under_base(base.as_ref(), user_path.as_ref())
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = (base, user_path);
        Err(FsPathError::new(
            "safe file creation is unsupported on this platform",
        ))
    }
}

/// Ensures a directory exists under `base` without following symlink ancestors.
///
/// On Unix each component is opened with `openat` plus
/// `O_DIRECTORY | O_NOFOLLOW`; missing components are created with `mkdirat`
/// and then opened using the same no-follow flags. This prevents a symlinked
/// cluster or level directory from redirecting later file creation outside the
/// Klei root. Non-Unix platforms fail closed until a handle-based no-follow
/// create implementation is available.
pub fn safe_ensure_dir_under_base(
    base: impl AsRef<Path>,
    user_path: impl AsRef<Path>,
) -> Result<(), FsPathError> {
    #[cfg(unix)]
    {
        safe_ensure_dir_under_base_unix(base.as_ref(), user_path.as_ref())
    }

    #[cfg(windows)]
    {
        windows::safe_ensure_dir_under_base(base.as_ref(), user_path.as_ref())
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = (base, user_path);
        Err(FsPathError::new(
            "safe directory creation is unsupported on this platform",
        ))
    }
}

/// Removes a directory tree under `base` without following symlink components.
///
/// Unix uses descriptor-anchored traversal and `unlinkat`, so symlink entries
/// inside the tree are unlinked as symlinks rather than followed. Non-Unix
/// platforms fail closed until an equivalent handle-based implementation is
/// added.
pub fn safe_remove_dir_all_under_base(
    base: impl AsRef<Path>,
    user_path: impl AsRef<Path>,
) -> Result<(), FsPathError> {
    #[cfg(unix)]
    {
        safe_remove_dir_all_under_base_unix(base.as_ref(), user_path.as_ref())
    }

    #[cfg(windows)]
    {
        windows::safe_remove_dir_all_under_base(base.as_ref(), user_path.as_ref())
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = (base, user_path);
        Err(FsPathError::new(
            "safe directory deletion is unsupported on this platform",
        ))
    }
}

/// Removes an optional regular file under `base` without following symlinks.
///
/// Missing parents or a missing leaf return `Ok(false)` after every reachable
/// parent component has been opened with `O_DIRECTORY | O_NOFOLLOW`. Symlink
/// ancestors, symlink leaves, and non-regular leaves are errors. This is the
/// file counterpart to [`safe_remove_dir_all_under_base`] for cleanup routes
/// that delete DST runtime logs while preserving descriptor-anchored path
/// traversal.
pub fn safe_remove_file_under_base(
    base: impl AsRef<Path>,
    user_path: impl AsRef<Path>,
) -> Result<bool, FsPathError> {
    #[cfg(unix)]
    {
        safe_remove_file_under_base_unix(base.as_ref(), user_path.as_ref())
    }

    #[cfg(windows)]
    {
        windows::safe_remove_file_under_base(base.as_ref(), user_path.as_ref())
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = (base, user_path);
        Err(FsPathError::new(
            "safe file deletion is unsupported on this platform",
        ))
    }
}

/// Renames a directory under `base` without following symlink components.
///
/// This is used for staged destructive operations: move the directory to an
/// internal tombstone name, commit the metadata change, then remove the
/// tombstone. Unix validates both parent paths with descriptor-anchored
/// traversal and verifies the source leaf is a directory with `fstatat` plus
/// `AT_SYMLINK_NOFOLLOW`. Non-Unix platforms fail closed until an equivalent
/// handle-based implementation is added.
pub fn safe_rename_dir_under_base(
    base: impl AsRef<Path>,
    from_path: impl AsRef<Path>,
    to_path: impl AsRef<Path>,
) -> Result<(), FsPathError> {
    #[cfg(unix)]
    {
        safe_rename_dir_under_base_unix(base.as_ref(), from_path.as_ref(), to_path.as_ref())
    }

    #[cfg(windows)]
    {
        windows::safe_rename_dir_under_base(base.as_ref(), from_path.as_ref(), to_path.as_ref())
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = (base, from_path, to_path);
        Err(FsPathError::new(
            "safe directory rename is unsupported on this platform",
        ))
    }
}

/// Renames a regular file under `base` without replacing an existing destination.
pub fn safe_rename_file_under_base(
    base: impl AsRef<Path>,
    from_path: impl AsRef<Path>,
    to_path: impl AsRef<Path>,
) -> Result<(), FsPathError> {
    #[cfg(unix)]
    {
        safe_rename_file_under_base_unix(base.as_ref(), from_path.as_ref(), to_path.as_ref())
    }

    #[cfg(windows)]
    {
        windows::safe_rename_file_under_base(base.as_ref(), from_path.as_ref(), to_path.as_ref())
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = (base, from_path, to_path);
        Err(FsPathError::new(
            "safe file rename is unsupported on this platform",
        ))
    }
}

/// Overwrites a regular file under `base` from a user-supplied relative path.
///
/// On Unix this uses the same descriptor-anchored `openat` traversal as the
/// read/create helpers and opens the leaf with `O_NOFOLLOW`; symlink ancestors,
/// symlink leaves, and non-file leaves are rejected before bytes are written.
/// Non-Unix platforms fail closed until an equivalent handle-based no-follow
/// implementation is added.
pub fn safe_overwrite_file_under_base(
    base: impl AsRef<Path>,
    user_path: impl AsRef<Path>,
    contents: impl AsRef<[u8]>,
) -> Result<(), FsPathError> {
    #[cfg(unix)]
    {
        safe_overwrite_file_under_base_unix(base.as_ref(), user_path.as_ref(), contents.as_ref())
    }

    #[cfg(not(unix))]
    {
        safe_overwrite_file_under_base_fallback(
            base.as_ref(),
            user_path.as_ref(),
            contents.as_ref(),
        )
    }
}

/// Overwrites a regular file at an absolute path without following symlink
/// ancestors or the leaf.
pub fn safe_overwrite_file_path(
    path: impl AsRef<Path>,
    contents: impl AsRef<[u8]>,
) -> Result<(), FsPathError> {
    #[cfg(unix)]
    {
        safe_overwrite_file_path_unix(path.as_ref(), contents.as_ref())
    }

    #[cfg(windows)]
    {
        windows::safe_overwrite_file_path(path.as_ref(), contents.as_ref())
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = (path, contents);
        Err(FsPathError::new(
            "safe file overwrite is unsupported on this platform",
        ))
    }
}

fn safe_directory_path_exists_inner(path: &Path) -> Result<bool, FsPathError> {
    if !path.is_absolute() {
        return Err(FsPathError::new("directory path must be absolute"));
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && parent != path
        && !safe_directory_path_exists_inner(parent)?
    {
        return Ok(false);
    }
    match path.symlink_metadata() {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || windows_reparse_point(&metadata) {
                return Err(FsPathError::new("directory path contains a symlink"));
            }
            if metadata.is_dir() {
                Ok(true)
            } else {
                Err(FsPathError::new("path is not a directory"))
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = safe_parent(path)?;
            safe_directory_path_exists_inner(parent).map(|_| false)
        }
        Err(_) => Err(FsPathError::new("directory path is unavailable")),
    }
}

fn safe_parent(path: &Path) -> Result<&Path, FsPathError> {
    let parent = path
        .parent()
        .ok_or_else(|| FsPathError::new("missing parent directory"))?;
    if parent.as_os_str().is_empty() || parent == path {
        return Err(FsPathError::new("missing parent directory"));
    }
    Ok(parent)
}

fn safe_leaf(path: &Path) -> Result<&str, FsPathError> {
    path.file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| FsPathError::new("path must end in a valid UTF-8 directory name"))
}

#[cfg(windows)]
fn safe_open_existing_file_under_base_fallback(
    base: &Path,
    user_path: &Path,
) -> Result<File, FsPathError> {
    safe_open_optional_existing_file_under_base_windows(base, user_path)?
        .ok_or_else(|| FsPathError::new("file is unavailable"))
}

#[cfg(windows)]
fn safe_open_optional_existing_file_under_base_fallback(
    base: &Path,
    user_path: &Path,
) -> Result<Option<File>, FsPathError> {
    safe_open_optional_existing_file_under_base_windows(base, user_path)
}

#[cfg(windows)]
fn safe_open_optional_existing_file_under_base_windows(
    base: &Path,
    user_path: &Path,
) -> Result<Option<File>, FsPathError> {
    use std::os::windows::fs::MetadataExt;

    let base = base
        .canonicalize()
        .map_err(|_| FsPathError::new("base directory is unavailable"))?;
    let components = safe_relative_components(user_path)?;
    let mut candidate = base.clone();
    for component in components {
        candidate.push(component);
        let metadata = match candidate.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(FsPathError::new("path is unavailable")),
        };
        // Reject symbolic links, junctions, and other reparse points. The
        // handle-path check below also protects against a replacement race.
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(FsPathError::new("path contains a reparse point"));
        }
    }

    let file = match File::open(&candidate) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(FsPathError::new("file is unavailable")),
    };
    if !file
        .metadata()
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
    {
        return Err(FsPathError::new("path is not a regular file"));
    }
    let opened_path = windows_final_path_by_handle(&file)?;
    if !opened_path.starts_with(&base) {
        return Err(FsPathError::new("path escapes base directory"));
    }
    Ok(Some(file))
}

#[cfg(windows)]
fn windows_final_path_by_handle(file: &File) -> Result<PathBuf, FsPathError> {
    use std::os::windows::io::AsRawHandle;

    windows_final_path_by_raw_handle(file.as_raw_handle())
}

#[cfg(windows)]
pub(super) fn windows_final_path_by_raw_handle(
    handle: std::os::windows::io::RawHandle,
) -> Result<PathBuf, FsPathError> {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFinalPathNameByHandleW(
            file: *mut std::ffi::c_void,
            path: *mut u16,
            path_len: u32,
            flags: u32,
        ) -> u32;
    }

    let mut path = vec![0u16; 260];
    loop {
        // SAFETY: the caller keeps `handle` live and `path` is a writable UTF-16
        // buffer whose length is supplied to the Windows API.
        let len =
            unsafe { GetFinalPathNameByHandleW(handle, path.as_mut_ptr(), path.len() as u32, 0) };
        if len == 0 {
            return Err(FsPathError::new("opened file path is unavailable"));
        }
        if (len as usize) < path.len() {
            return Ok(PathBuf::from(OsString::from_wide(&path[..len as usize])));
        }
        path.resize(len as usize + 1, 0);
    }
}

#[cfg(all(not(unix), not(windows)))]
fn safe_open_existing_file_under_base_fallback(
    base: &Path,
    user_path: &Path,
) -> Result<File, FsPathError> {
    let _ = (base, user_path);
    Err(FsPathError::new(
        "safe file opening is unsupported on this platform",
    ))
}

#[cfg(all(not(unix), not(windows)))]
fn safe_open_optional_existing_file_under_base_fallback(
    base: &Path,
    user_path: &Path,
) -> Result<Option<File>, FsPathError> {
    let _ = (base, user_path);
    Err(FsPathError::new(
        "safe optional file opening is unsupported on this platform",
    ))
}

#[cfg(windows)]
fn safe_overwrite_file_under_base_fallback(
    base: &Path,
    user_path: &Path,
    contents: &[u8],
) -> Result<(), FsPathError> {
    windows::safe_overwrite_file_under_base(base, user_path, contents)
}

#[cfg(all(not(unix), not(windows)))]
fn safe_overwrite_file_under_base_fallback(
    base: &Path,
    user_path: &Path,
    contents: &[u8],
) -> Result<(), FsPathError> {
    let _ = (base, user_path, contents);
    Err(FsPathError::new(
        "safe file overwrite is unsupported on this platform",
    ))
}

#[cfg(unix)]
fn safe_ensure_dir_under_base_unix(base: &Path, user_path: &Path) -> Result<(), FsPathError> {
    let base = base
        .canonicalize()
        .map_err(|_| FsPathError::new("base directory is unavailable"))?;
    let components = safe_relative_components(user_path)?;
    let mut current_dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&base)
        .map_err(|_| FsPathError::new("base directory is unavailable"))?;

    for component in components {
        match openat_directory(current_dir.as_raw_fd(), &component) {
            Ok(next_fd) => {
                // SAFETY: `openat_directory` returns a fresh owned directory fd.
                current_dir = unsafe { File::from_raw_fd(next_fd) };
            }
            Err(_) => {
                mkdirat_directory(current_dir.as_raw_fd(), &component)?;
                let next_fd = openat_directory(current_dir.as_raw_fd(), &component)?;
                // SAFETY: `openat_directory` returns a fresh owned directory fd.
                current_dir = unsafe { File::from_raw_fd(next_fd) };
            }
        }
    }

    Ok(())
}

#[cfg(unix)]
fn safe_directory_exists_under_base_unix(
    base: &Path,
    user_path: &Path,
) -> Result<bool, FsPathError> {
    let base = base
        .canonicalize()
        .map_err(|_| FsPathError::new("base directory is unavailable"))?;
    let components = safe_relative_components(user_path)?;
    let mut current_dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&base)
        .map_err(|_| FsPathError::new("base directory is unavailable"))?;

    for component in components {
        let Some(next_fd) = openat_optional_directory(current_dir.as_raw_fd(), &component)? else {
            return Ok(false);
        };
        // SAFETY: `openat_optional_directory` returns a fresh owned descriptor.
        current_dir = unsafe { File::from_raw_fd(next_fd) };
    }

    Ok(true)
}

#[cfg(unix)]
fn safe_remove_dir_all_under_base_unix(base: &Path, user_path: &Path) -> Result<(), FsPathError> {
    let base = base
        .canonicalize()
        .map_err(|_| FsPathError::new("base directory is unavailable"))?;
    let components = safe_relative_components(user_path)?;
    let (leaf, ancestors) = components
        .split_last()
        .ok_or_else(|| FsPathError::new("path cannot be empty"))?;

    let mut current_dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&base)
        .map_err(|_| FsPathError::new("base directory is unavailable"))?;

    for ancestor in ancestors {
        let next_fd = openat_directory(current_dir.as_raw_fd(), ancestor)?;
        // SAFETY: `openat_directory` returns a fresh owned descriptor.
        current_dir = unsafe { File::from_raw_fd(next_fd) };
    }

    let leaf_fd = openat_directory(current_dir.as_raw_fd(), leaf)?;
    remove_open_directory_contents(leaf_fd)?;
    unlinkat_directory(current_dir.as_raw_fd(), leaf)
}

#[cfg(unix)]
fn safe_remove_file_under_base_unix(base: &Path, user_path: &Path) -> Result<bool, FsPathError> {
    let base = base
        .canonicalize()
        .map_err(|_| FsPathError::new("base directory is unavailable"))?;
    let components = safe_relative_components(user_path)?;
    let (leaf, ancestors) = components
        .split_last()
        .ok_or_else(|| FsPathError::new("path cannot be empty"))?;

    let mut current_dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&base)
        .map_err(|_| FsPathError::new("base directory is unavailable"))?;

    for ancestor in ancestors {
        let Some(next_fd) = openat_optional_directory(current_dir.as_raw_fd(), ancestor)? else {
            return Ok(false);
        };
        // SAFETY: `openat_optional_directory` returns a fresh owned descriptor.
        current_dir = unsafe { File::from_raw_fd(next_fd) };
    }

    let leaf = CString::new(leaf.as_str())
        .map_err(|_| FsPathError::new("path contains unsafe characters"))?;
    let Some(metadata) = fstatat_no_follow_optional(current_dir.as_raw_fd(), &leaf)? else {
        return Ok(false);
    };
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(FsPathError::new("path is not a regular file"));
    }

    unlinkat_entry(current_dir.as_raw_fd(), &leaf, 0)?;
    Ok(true)
}

#[cfg(unix)]
fn safe_rename_dir_under_base_unix(
    base: &Path,
    from_path: &Path,
    to_path: &Path,
) -> Result<(), FsPathError> {
    let base = base
        .canonicalize()
        .map_err(|_| FsPathError::new("base directory is unavailable"))?;
    let from_components = safe_relative_components(from_path)?;
    let to_components = safe_relative_components(to_path)?;
    let (from_leaf, from_ancestors) = from_components
        .split_last()
        .ok_or_else(|| FsPathError::new("path cannot be empty"))?;
    let (to_leaf, to_ancestors) = to_components
        .split_last()
        .ok_or_else(|| FsPathError::new("path cannot be empty"))?;

    let from_parent = open_directory_components_under_base(&base, from_ancestors)?;
    let to_parent = open_directory_components_under_base(&base, to_ancestors)?;
    let from_leaf = CString::new(from_leaf.as_str())
        .map_err(|_| FsPathError::new("path contains unsafe characters"))?;
    let to_leaf = CString::new(to_leaf.as_str())
        .map_err(|_| FsPathError::new("path contains unsafe characters"))?;

    let source = fstatat_no_follow(from_parent.as_raw_fd(), &from_leaf)?;
    if source.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(FsPathError::new("source path is not a directory"));
    }
    renameat_directory_noreplace(
        from_parent.as_raw_fd(),
        &from_leaf,
        to_parent.as_raw_fd(),
        &to_leaf,
    )
}

#[cfg(unix)]
fn safe_rename_file_under_base_unix(
    base: &Path,
    from_path: &Path,
    to_path: &Path,
) -> Result<(), FsPathError> {
    let base = base
        .canonicalize()
        .map_err(|_| FsPathError::new("base directory is unavailable"))?;
    let from_components = safe_relative_components(from_path)?;
    let to_components = safe_relative_components(to_path)?;
    let (from_leaf, from_ancestors) = from_components
        .split_last()
        .ok_or_else(|| FsPathError::new("path cannot be empty"))?;
    let (to_leaf, to_ancestors) = to_components
        .split_last()
        .ok_or_else(|| FsPathError::new("path cannot be empty"))?;

    let from_parent = open_directory_components_under_base(&base, from_ancestors)?;
    let to_parent = open_directory_components_under_base(&base, to_ancestors)?;
    let from_leaf = CString::new(from_leaf.as_str())
        .map_err(|_| FsPathError::new("path contains unsafe characters"))?;
    let to_leaf = CString::new(to_leaf.as_str())
        .map_err(|_| FsPathError::new("path contains unsafe characters"))?;

    let source = fstatat_no_follow(from_parent.as_raw_fd(), &from_leaf)?;
    if source.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(FsPathError::new("source path is not a regular file"));
    }
    renameat_directory_noreplace(
        from_parent.as_raw_fd(),
        &from_leaf,
        to_parent.as_raw_fd(),
        &to_leaf,
    )
}

#[cfg(unix)]
fn safe_open_existing_file_under_base_unix(
    base: &Path,
    user_path: &Path,
) -> Result<File, FsPathError> {
    let leaf_fd = open_leaf_under_base_unix(base, user_path, LeafOpenMode::Existing)?;
    // SAFETY: `open_leaf_under_base_unix` returns a fresh owned descriptor for
    // the opened leaf. Returning `File` transfers descriptor ownership.
    let file = unsafe { File::from_raw_fd(leaf_fd) };
    let metadata = file
        .metadata()
        .map_err(|_| FsPathError::new("path is unavailable"))?;
    if metadata.is_file() {
        Ok(file)
    } else {
        Err(FsPathError::new("path is not a file"))
    }
}

#[cfg(unix)]
fn safe_open_existing_file_path_unix(path: &Path) -> Result<File, FsPathError> {
    let leaf_fd = open_leaf_absolute_unix(path, LeafOpenMode::Existing)?;
    // SAFETY: `open_leaf_absolute_unix` returns a fresh owned descriptor.
    let file = unsafe { File::from_raw_fd(leaf_fd) };
    let metadata = file
        .metadata()
        .map_err(|_| FsPathError::new("path is unavailable"))?;
    if metadata.is_file() {
        Ok(file)
    } else {
        Err(FsPathError::new("path is not a file"))
    }
}

#[cfg(unix)]
fn safe_open_optional_existing_file_under_base_unix(
    base: &Path,
    user_path: &Path,
) -> Result<Option<File>, FsPathError> {
    let leaf_fd = open_optional_leaf_under_base_unix(base, user_path)?;
    let Some(leaf_fd) = leaf_fd else {
        return Ok(None);
    };
    // SAFETY: `open_optional_leaf_under_base_unix` returns a fresh owned
    // descriptor for the opened leaf. Returning `File` transfers ownership.
    let file = unsafe { File::from_raw_fd(leaf_fd) };
    let metadata = file
        .metadata()
        .map_err(|_| FsPathError::new("path is unavailable"))?;
    if metadata.is_file() {
        Ok(Some(file))
    } else {
        Err(FsPathError::new("path is not a file"))
    }
}

#[cfg(unix)]
fn safe_open_optional_existing_file_path_unix(path: &Path) -> Result<Option<File>, FsPathError> {
    let leaf_fd = open_optional_leaf_absolute_unix(path)?;
    let Some(leaf_fd) = leaf_fd else {
        return Ok(None);
    };
    // SAFETY: `open_optional_leaf_absolute_unix` returns a fresh owned descriptor.
    let file = unsafe { File::from_raw_fd(leaf_fd) };
    let metadata = file
        .metadata()
        .map_err(|_| FsPathError::new("path is unavailable"))?;
    if metadata.is_file() {
        Ok(Some(file))
    } else {
        Err(FsPathError::new("path is not a file"))
    }
}

#[cfg(unix)]
fn safe_create_new_file_under_base_unix(
    base: &Path,
    user_path: &Path,
) -> Result<File, FsPathError> {
    let leaf_fd = open_leaf_under_base_unix(base, user_path, LeafOpenMode::CreateNew)?;
    // SAFETY: `open_leaf_under_base_unix` returns a fresh owned descriptor for
    // the newly created leaf. Returning `File` transfers ownership to caller.
    Ok(unsafe { File::from_raw_fd(leaf_fd) })
}

#[cfg(unix)]
fn safe_overwrite_file_under_base_unix(
    base: &Path,
    user_path: &Path,
    contents: &[u8],
) -> Result<(), FsPathError> {
    let leaf_fd = open_leaf_under_base_unix(base, user_path, LeafOpenMode::Overwrite)?;
    // SAFETY: `open_leaf_under_base_unix` returns a fresh owned descriptor for
    // the opened leaf. Wrapping it in `File` transfers descriptor ownership.
    let mut file = unsafe { File::from_raw_fd(leaf_fd) };
    let metadata = file
        .metadata()
        .map_err(|_| FsPathError::new("path is unavailable"))?;
    if !metadata.is_file() {
        return Err(FsPathError::new("path is not a file"));
    }
    file.set_len(0)
        .map_err(|_| FsPathError::new("file could not be truncated safely"))?;
    file.write_all(contents)
        .map_err(|_| FsPathError::new("file could not be written safely"))
}

#[cfg(unix)]
fn safe_overwrite_file_path_unix(path: &Path, contents: &[u8]) -> Result<(), FsPathError> {
    let leaf_fd = open_leaf_absolute_unix(path, LeafOpenMode::Overwrite)?;
    // SAFETY: `open_leaf_absolute_unix` returns a fresh owned descriptor.
    let mut file = unsafe { File::from_raw_fd(leaf_fd) };
    let metadata = file
        .metadata()
        .map_err(|_| FsPathError::new("path is unavailable"))?;
    if !metadata.is_file() {
        return Err(FsPathError::new("path is not a file"));
    }
    file.set_len(0)
        .map_err(|_| FsPathError::new("file could not be truncated safely"))?;
    file.write_all(contents)
        .map_err(|_| FsPathError::new("file could not be written safely"))
}

#[cfg(unix)]
fn open_optional_leaf_absolute_unix(path: &Path) -> Result<Option<RawFd>, FsPathError> {
    let components = absolute_normal_components(path)?;
    let (leaf, ancestors) = components
        .split_last()
        .ok_or_else(|| FsPathError::new("path cannot be empty"))?;
    let mut current_dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(Path::new("/"))
        .map_err(|_| FsPathError::new("base directory is unavailable"))?;

    for ancestor in ancestors {
        let Some(next_fd) = openat_optional_directory(current_dir.as_raw_fd(), ancestor)? else {
            return Ok(None);
        };
        // SAFETY: `openat_optional_directory` returns a fresh owned descriptor.
        current_dir = unsafe { File::from_raw_fd(next_fd) };
    }

    openat_optional_existing_file(current_dir.as_raw_fd(), leaf)
}

#[cfg(unix)]
fn open_leaf_absolute_unix(path: &Path, mode: LeafOpenMode) -> Result<RawFd, FsPathError> {
    let components = absolute_normal_components(path)?;
    let (leaf, ancestors) = components
        .split_last()
        .ok_or_else(|| FsPathError::new("path cannot be empty"))?;
    let mut current_dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(Path::new("/"))
        .map_err(|_| FsPathError::new("base directory is unavailable"))?;

    for ancestor in ancestors {
        let next_fd = openat_directory(current_dir.as_raw_fd(), ancestor)?;
        // SAFETY: `openat_directory` returns a fresh owned descriptor.
        current_dir = unsafe { File::from_raw_fd(next_fd) };
    }

    match mode {
        LeafOpenMode::Existing => openat_existing_file(current_dir.as_raw_fd(), leaf),
        LeafOpenMode::CreateNew => openat_new_file(current_dir.as_raw_fd(), leaf),
        LeafOpenMode::Overwrite => openat_overwrite_file(current_dir.as_raw_fd(), leaf),
    }
}

#[cfg(unix)]
fn open_optional_leaf_under_base_unix(
    base: &Path,
    user_path: &Path,
) -> Result<Option<RawFd>, FsPathError> {
    let base = base
        .canonicalize()
        .map_err(|_| FsPathError::new("base directory is unavailable"))?;
    let components = safe_relative_components(user_path)?;
    let (leaf, ancestors) = components
        .split_last()
        .ok_or_else(|| FsPathError::new("path cannot be empty"))?;

    let mut current_dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&base)
        .map_err(|_| FsPathError::new("base directory is unavailable"))?;

    for ancestor in ancestors {
        let Some(next_fd) = openat_optional_directory(current_dir.as_raw_fd(), ancestor)? else {
            return Ok(None);
        };
        // SAFETY: `openat_directory` returns a fresh owned descriptor.
        current_dir = unsafe { File::from_raw_fd(next_fd) };
    }

    openat_optional_existing_file(current_dir.as_raw_fd(), leaf)
}

#[cfg(unix)]
enum LeafOpenMode {
    Existing,
    CreateNew,
    Overwrite,
}

#[cfg(unix)]
fn open_leaf_under_base_unix(
    base: &Path,
    user_path: &Path,
    mode: LeafOpenMode,
) -> Result<RawFd, FsPathError> {
    let base = base
        .canonicalize()
        .map_err(|_| FsPathError::new("base directory is unavailable"))?;
    let components = safe_relative_components(user_path)?;
    let (leaf, ancestors) = components
        .split_last()
        .ok_or_else(|| FsPathError::new("path cannot be empty"))?;

    let mut current_dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&base)
        .map_err(|_| FsPathError::new("base directory is unavailable"))?;

    for ancestor in ancestors {
        let next_fd = openat_directory(current_dir.as_raw_fd(), ancestor)?;
        // SAFETY: `openat_directory` returns a fresh owned descriptor on
        // success. Wrapping it in `File` transfers that ownership so it will be
        // closed exactly once when `current_dir` is replaced or dropped.
        current_dir = unsafe { File::from_raw_fd(next_fd) };
    }

    match mode {
        LeafOpenMode::Existing => openat_existing_file(current_dir.as_raw_fd(), leaf),
        LeafOpenMode::CreateNew => openat_new_file(current_dir.as_raw_fd(), leaf),
        LeafOpenMode::Overwrite => openat_overwrite_file(current_dir.as_raw_fd(), leaf),
    }
}

#[cfg(unix)]
fn open_directory_components_under_base(
    base: &Path,
    components: &[String],
) -> Result<File, FsPathError> {
    let mut current_dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(base)
        .map_err(|_| FsPathError::new("base directory is unavailable"))?;

    for component in components {
        let next_fd = openat_directory(current_dir.as_raw_fd(), component)?;
        // SAFETY: `openat_directory` returns a fresh owned descriptor.
        current_dir = unsafe { File::from_raw_fd(next_fd) };
    }

    Ok(current_dir)
}

#[cfg(unix)]
fn openat_optional_directory(
    parent_fd: RawFd,
    component: &str,
) -> Result<Option<RawFd>, FsPathError> {
    let component =
        CString::new(component).map_err(|_| FsPathError::new("path contains unsafe characters"))?;
    // SAFETY: `parent_fd` is an open directory descriptor owned by `File` in
    // the caller, and `component` is converted to a NUL-free C string. A
    // missing ancestor means the optional leaf is absent; every other failure
    // still represents an unsafe or unavailable parent.
    let fd = unsafe {
        libc::openat(
            parent_fd,
            component.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd >= 0 {
        Ok(Some(fd))
    } else if io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
        Ok(None)
    } else {
        Err(FsPathError::new("parent directory is unavailable"))
    }
}

#[cfg(unix)]
fn openat_directory(parent_fd: RawFd, component: &str) -> Result<RawFd, FsPathError> {
    let component =
        CString::new(component).map_err(|_| FsPathError::new("path contains unsafe characters"))?;
    // SAFETY: `parent_fd` is an open directory descriptor owned by `File` in
    // the caller, and `component` is converted to a NUL-free C string. The
    // returned fd is checked before ownership is transferred.
    let fd = unsafe {
        libc::openat(
            parent_fd,
            component.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        Err(FsPathError::new("parent directory is unavailable"))
    } else {
        Ok(fd)
    }
}

#[cfg(unix)]
fn mkdirat_directory(parent_fd: RawFd, component: &str) -> Result<(), FsPathError> {
    let component =
        CString::new(component).map_err(|_| FsPathError::new("path contains unsafe characters"))?;
    // SAFETY: `parent_fd` is an open directory descriptor and `component` is a
    // validated, NUL-free single path component.
    let result = unsafe { libc::mkdirat(parent_fd, component.as_ptr(), 0o755) };
    if result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST) {
        Ok(())
    } else {
        Err(FsPathError::new("directory could not be created safely"))
    }
}

#[cfg(unix)]
fn remove_open_directory_contents(dir_fd: RawFd) -> Result<(), FsPathError> {
    // SAFETY: `dir_fd` is a fresh descriptor returned by an `openat` helper.
    // Wrapping it immediately makes every early-return path close the directory
    // instead of depending on manual cleanup in each error branch.
    let dir_file = unsafe { File::from_raw_fd(dir_fd) };
    let dir = DirStream::open(dir_file.as_raw_fd())?;

    loop {
        // SAFETY: `dir` owns a valid DIR* until it is closed or dropped.
        let entry = unsafe { libc::readdir(dir.as_ptr()) };
        if entry.is_null() {
            break;
        }
        // SAFETY: `d_name` is a NUL-terminated C string provided by `readdir`.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }

        let metadata = fstatat_no_follow(dir_file.as_raw_fd(), name)?;
        if metadata.st_mode & libc::S_IFMT == libc::S_IFDIR {
            let child_fd = openat_directory_cstr(dir_file.as_raw_fd(), name)?;
            remove_open_directory_contents(child_fd)?;
            unlinkat_entry(dir_file.as_raw_fd(), name, libc::AT_REMOVEDIR)?;
        } else {
            unlinkat_entry(dir_file.as_raw_fd(), name, 0)?;
        }
    }

    dir.close()
}

#[cfg(unix)]
struct DirStream {
    dir: *mut libc::DIR,
}

#[cfg(unix)]
impl DirStream {
    fn open(parent_fd: RawFd) -> Result<Self, FsPathError> {
        // SAFETY: `dup` returns a new descriptor for `fdopendir` to own; the
        // caller keeps `parent_fd` open for `openat`, `fstatat`, and `unlinkat`.
        let iter_fd = unsafe { libc::dup(parent_fd) };
        if iter_fd < 0 {
            return Err(FsPathError::new("directory could not be read safely"));
        }

        // SAFETY: `iter_fd` is a fresh directory descriptor. `fdopendir` takes
        // ownership on success, and `DirStream` will close it through `closedir`.
        let dir = unsafe { libc::fdopendir(iter_fd) };
        if dir.is_null() {
            // SAFETY: no `DIR*` owns `iter_fd` when `fdopendir` fails.
            unsafe {
                libc::close(iter_fd);
            }
            return Err(FsPathError::new("directory could not be read safely"));
        }

        Ok(Self { dir })
    }

    fn as_ptr(&self) -> *mut libc::DIR {
        self.dir
    }

    fn close(mut self) -> Result<(), FsPathError> {
        let dir = self.dir;
        self.dir = std::ptr::null_mut();
        // SAFETY: `dir` is the valid DIR* owned by this guard. Nulling the field
        // before the call prevents `Drop` from closing it a second time.
        let close_result = unsafe { libc::closedir(dir) };
        if close_result == 0 {
            Ok(())
        } else {
            Err(FsPathError::new("directory could not be closed safely"))
        }
    }
}

#[cfg(unix)]
impl Drop for DirStream {
    fn drop(&mut self) {
        if !self.dir.is_null() {
            // SAFETY: this guard owns the DIR* while non-null. Drop is a fallback
            // cleanup path, so close errors cannot be surfaced here.
            unsafe {
                libc::closedir(self.dir);
            }
        }
    }
}

#[cfg(unix)]
fn fstatat_no_follow(parent_fd: RawFd, component: &CStr) -> Result<libc::stat, FsPathError> {
    // SAFETY: zeroed `stat` is filled by `fstatat` before being read.
    let mut metadata = unsafe { mem::zeroed::<libc::stat>() };
    // SAFETY: `parent_fd` is an open directory descriptor and `component` is a
    // NUL-terminated entry name from `readdir`.
    let result = unsafe {
        libc::fstatat(
            parent_fd,
            component.as_ptr(),
            &mut metadata,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(metadata)
    } else {
        Err(FsPathError::new("directory entry is unavailable"))
    }
}

#[cfg(unix)]
fn fstatat_no_follow_optional(
    parent_fd: RawFd,
    component: &CStr,
) -> Result<Option<libc::stat>, FsPathError> {
    // SAFETY: zeroed `stat` is filled by `fstatat` before being read on
    // success. The `AT_SYMLINK_NOFOLLOW` flag makes symlink leaves visible to
    // the caller as symlinks rather than resolving them.
    let mut metadata = unsafe { mem::zeroed::<libc::stat>() };
    // SAFETY: `parent_fd` is an open directory descriptor and `component` is a
    // NUL-terminated single path component.
    let result = unsafe {
        libc::fstatat(
            parent_fd,
            component.as_ptr(),
            &mut metadata,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(Some(metadata))
    } else if io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
        Ok(None)
    } else {
        Err(FsPathError::new("directory entry is unavailable"))
    }
}

#[cfg(all(unix, target_os = "linux"))]
fn renameat_directory_noreplace(
    from_parent_fd: RawFd,
    from_leaf: &CStr,
    to_parent_fd: RawFd,
    to_leaf: &CStr,
) -> Result<(), FsPathError> {
    // SAFETY: both parent descriptors were opened through no-follow traversal,
    // and both leaves are validated single path components. `RENAME_NOREPLACE`
    // makes the destination non-existence check atomic with the rename.
    let result = unsafe {
        libc::renameat2(
            from_parent_fd,
            from_leaf.as_ptr(),
            to_parent_fd,
            to_leaf.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    renameat_noreplace_result(result)
}

#[cfg(all(unix, target_os = "macos"))]
fn renameat_directory_noreplace(
    from_parent_fd: RawFd,
    from_leaf: &CStr,
    to_parent_fd: RawFd,
    to_leaf: &CStr,
) -> Result<(), FsPathError> {
    // SAFETY: both parent descriptors were opened through no-follow traversal,
    // and both leaves are validated single path components. `RENAME_EXCL` makes
    // the destination non-existence check atomic with the rename on Darwin.
    let result = unsafe {
        libc::renameatx_np(
            from_parent_fd,
            from_leaf.as_ptr(),
            to_parent_fd,
            to_leaf.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    renameat_noreplace_result(result)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn renameat_directory_noreplace(
    from_parent_fd: RawFd,
    from_leaf: &CStr,
    to_parent_fd: RawFd,
    to_leaf: &CStr,
) -> Result<(), FsPathError> {
    let _ = (from_parent_fd, from_leaf, to_parent_fd, to_leaf);
    Err(FsPathError::new(
        "atomic no-replace directory rename is unsupported on this platform",
    ))
}

#[cfg(unix)]
fn renameat_noreplace_result(result: libc::c_int) -> Result<(), FsPathError> {
    if result == 0 {
        return Ok(());
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(libc::EEXIST) => Err(FsPathError::new("destination path already exists")),
        _ => Err(FsPathError::new("directory could not be renamed safely")),
    }
}

#[cfg(unix)]
fn openat_directory_cstr(parent_fd: RawFd, component: &CStr) -> Result<RawFd, FsPathError> {
    // SAFETY: `parent_fd` is an open directory descriptor and `component` is a
    // NUL-terminated entry name from `readdir`.
    let fd = unsafe {
        libc::openat(
            parent_fd,
            component.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        Err(FsPathError::new("parent directory is unavailable"))
    } else {
        Ok(fd)
    }
}

#[cfg(unix)]
fn unlinkat_entry(parent_fd: RawFd, component: &CStr, flags: i32) -> Result<(), FsPathError> {
    // SAFETY: `parent_fd` is an open directory descriptor and `component` is a
    // NUL-terminated entry name from `readdir`.
    let result = unsafe { libc::unlinkat(parent_fd, component.as_ptr(), flags) };
    if result == 0 {
        Ok(())
    } else {
        Err(FsPathError::new(
            "directory entry could not be removed safely",
        ))
    }
}

#[cfg(unix)]
fn unlinkat_directory(parent_fd: RawFd, component: &str) -> Result<(), FsPathError> {
    let component =
        CString::new(component).map_err(|_| FsPathError::new("path contains unsafe characters"))?;
    unlinkat_entry(parent_fd, &component, libc::AT_REMOVEDIR)
}

#[cfg(unix)]
fn openat_optional_existing_file(
    parent_fd: RawFd,
    component: &str,
) -> Result<Option<RawFd>, FsPathError> {
    let component =
        CString::new(component).map_err(|_| FsPathError::new("path contains unsafe characters"))?;
    // SAFETY: `parent_fd` is an open directory descriptor owned by `File` in
    // the caller, and `component` is a NUL-free C string.
    let fd = unsafe {
        libc::openat(
            parent_fd,
            component.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd >= 0 {
        Ok(Some(fd))
    } else if io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
        Ok(None)
    } else {
        Err(FsPathError::new("path is unavailable"))
    }
}

#[cfg(unix)]
fn openat_existing_file(parent_fd: RawFd, component: &str) -> Result<RawFd, FsPathError> {
    let component =
        CString::new(component).map_err(|_| FsPathError::new("path contains unsafe characters"))?;
    // SAFETY: `parent_fd` is an open directory descriptor owned by `File` in
    // the caller, and `component` is a NUL-free C string. `O_NONBLOCK` prevents
    // FIFO or device leaves from blocking before the caller can `fstat` and
    // reject non-regular files; it does not change normal file reads.
    let fd = unsafe {
        libc::openat(
            parent_fd,
            component.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        Err(FsPathError::new("path is unavailable"))
    } else {
        Ok(fd)
    }
}

#[cfg(unix)]
fn openat_new_file(parent_fd: RawFd, component: &str) -> Result<RawFd, FsPathError> {
    let component =
        CString::new(component).map_err(|_| FsPathError::new("path contains unsafe characters"))?;
    // SAFETY: `parent_fd` is an open directory descriptor owned by `File` in
    // the caller, and `component` is converted to a NUL-free C string. The
    // returned fd is checked before ownership is transferred.
    let fd = unsafe {
        libc::openat(
            parent_fd,
            component.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o644,
        )
    };
    if fd < 0 {
        Err(FsPathError::new("file could not be created safely"))
    } else {
        Ok(fd)
    }
}

#[cfg(unix)]
fn openat_overwrite_file(parent_fd: RawFd, component: &str) -> Result<RawFd, FsPathError> {
    let component =
        CString::new(component).map_err(|_| FsPathError::new("path contains unsafe characters"))?;
    // SAFETY: `parent_fd` is an open directory descriptor owned by `File` in
    // the caller, and `component` is a NUL-free C string. `O_NONBLOCK` avoids
    // blocking on FIFO/device leaves before the caller rejects non-files. The
    // file is truncated only after `metadata().is_file()` succeeds.
    let fd = unsafe {
        libc::openat(
            parent_fd,
            component.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0o644,
        )
    };
    if fd < 0 {
        Err(FsPathError::new("file could not be opened safely"))
    } else {
        Ok(fd)
    }
}

fn safe_relative_components(path: &Path) -> Result<Vec<String>, FsPathError> {
    reject_normalized_away_components(path)?;

    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value
                    .to_str()
                    .ok_or_else(|| FsPathError::new("path must be valid UTF-8"))?;
                validate_component(value)?;
                parts.push(value.to_owned());
            }
            Component::CurDir => {
                return Err(FsPathError::new(
                    "current-directory components are not allowed",
                ));
            }
            Component::ParentDir => {
                return Err(FsPathError::new(
                    "parent-directory traversal is not allowed",
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(FsPathError::new("absolute paths are not allowed"));
            }
        }
    }
    if parts.is_empty() {
        return Err(FsPathError::new("path cannot be empty"));
    }
    Ok(parts)
}

fn absolute_normal_components(path: &Path) -> Result<Vec<String>, FsPathError> {
    validate_absolute_path(path)?;
    let mut parts = Vec::new();
    for component in path.components() {
        if let Component::Normal(value) = component {
            let value = value
                .to_str()
                .ok_or_else(|| FsPathError::new("path must be valid UTF-8"))?;
            validate_component(value)?;
            parts.push(value.to_owned());
        }
    }
    if parts.is_empty() {
        return Err(FsPathError::new("path cannot be empty"));
    }
    Ok(parts)
}

#[cfg(not(windows))]
fn validate_absolute_path(path: &Path) -> Result<(), FsPathError> {
    if !path.is_absolute() {
        return Err(FsPathError::new("directory path must be absolute"));
    }
    let raw = path
        .to_str()
        .ok_or_else(|| FsPathError::new("path must be valid UTF-8"))?;
    if raw.contains('\\') {
        return Err(FsPathError::new(
            "backslash path separators are not allowed",
        ));
    }
    // Absolute paths have one leading empty split component. Any additional empty
    // component would be normalized away by the OS path resolver and is rejected
    // so callers cannot smuggle alternate traversal semantics into checked paths.
    for component in raw.split('/').skip(1) {
        if component.is_empty() {
            return Err(FsPathError::new("path components cannot be empty"));
        }
        if component == "." {
            return Err(FsPathError::new(
                "current-directory components are not allowed",
            ));
        }
        if component == ".." {
            return Err(FsPathError::new(
                "parent-directory traversal is not allowed",
            ));
        }
    }

    let mut normal_components = 0usize;
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value
                    .to_str()
                    .ok_or_else(|| FsPathError::new("path must be valid UTF-8"))?;
                validate_component(value)?;
                normal_components += 1;
            }
            Component::RootDir | Component::Prefix(_) => {}
            Component::CurDir => {
                return Err(FsPathError::new(
                    "current-directory components are not allowed",
                ));
            }
            Component::ParentDir => {
                return Err(FsPathError::new(
                    "parent-directory traversal is not allowed",
                ));
            }
        }
    }
    if normal_components == 0 {
        return Err(FsPathError::new("path cannot be empty"));
    }
    Ok(())
}

#[cfg(windows)]
fn validate_absolute_path(path: &Path) -> Result<(), FsPathError> {
    if !path.is_absolute() {
        return Err(FsPathError::new("directory path must be absolute"));
    }
    let raw = path
        .to_str()
        .ok_or_else(|| FsPathError::new("path must be valid UTF-8"))?;
    if raw
        .split(['/', '\\'])
        .any(|part| part == "." || part == "..")
    {
        return Err(FsPathError::new("relative components are not allowed"));
    }
    let mut normal_components = 0;
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                validate_component(
                    value
                        .to_str()
                        .ok_or_else(|| FsPathError::new("path must be valid UTF-8"))?,
                )?;
                normal_components += 1;
            }
            Component::RootDir | Component::Prefix(_) => {}
            Component::CurDir | Component::ParentDir => {
                return Err(FsPathError::new("relative components are not allowed"));
            }
        }
    }
    if normal_components == 0 && path.parent().is_none() {
        return Err(FsPathError::new("path cannot be empty"));
    }
    Ok(())
}

fn reject_normalized_away_components(path: &Path) -> Result<(), FsPathError> {
    let raw = path
        .to_str()
        .ok_or_else(|| FsPathError::new("path must be valid UTF-8"))?;
    if raw.is_empty() {
        return Err(FsPathError::new("path cannot be empty"));
    }
    // Unix treats backslashes as ordinary filename bytes, unlike Windows.
    // Reject them there so a checked path cannot change meaning across hosts.
    if !cfg!(windows) && raw.contains('\\') {
        return Err(FsPathError::new(
            "backslash path separators are not allowed",
        ));
    }
    if raw.split(['/', '\\']).any(|component| component == ".") {
        return Err(FsPathError::new(
            "current-directory components are not allowed",
        ));
    }
    if raw.split(['/', '\\']).any(str::is_empty) {
        return Err(FsPathError::new("path components cannot be empty"));
    }
    Ok(())
}

fn validate_component(value: &str) -> Result<(), FsPathError> {
    if value.trim().is_empty() {
        return Err(FsPathError::new("path components cannot be empty"));
    }
    if value == "." || value == ".." {
        return Err(FsPathError::new("reserved path component"));
    }
    if value
        .chars()
        .any(|ch| ch == '/' || ch == '\\' || ch.is_control())
    {
        return Err(FsPathError::new("path contains unsafe characters"));
    }
    if cfg!(windows) && (value.contains(':') || value.ends_with('.') || value.ends_with(' ')) {
        return Err(FsPathError::new("path contains unsafe Windows characters"));
    }
    Ok(())
}

#[cfg(windows)]
fn windows_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn windows_reparse_point(_metadata: &std::fs::Metadata) -> bool {
    false
}
