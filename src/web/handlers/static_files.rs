//! Static-file fallback handlers for the migrated app shell.
//!
//! Missing static assets are treated as normal 404s so development and test
//! environments can run without frontend build artifacts. All paths are
//! resolved under `root_path/dist` to keep the migrated server from serving
//! arbitrary files when users request crafted paths or symlinks.

use std::{
    fs, io,
    io::Read,
    path::{Path, PathBuf},
};

use axum::{
    body::Body,
    extract::State,
    http::{
        HeaderValue, Method, StatusCode, Uri,
        header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
};

use crate::{
    infra::fs_paths::safe_open_existing_file_under_base,
    web::app::AppState,
    web::error::{AppError, AppResult},
};

/// Serves `dist/index.html` from the configured application root.
pub(crate) async fn index_handler(State(state): State<AppState>) -> AppResult<Response> {
    let Some(asset) = open_dist_asset(&state.root_path, "/index.html")? else {
        tracing::warn!("rejected unsafe static index fallback path");
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    match read_static_asset_body(asset.file) {
        Ok(contents) => {
            tracing::debug!(
                index_path = %asset.display_path.display(),
                bytes = contents.len(),
                "serving static index fallback"
            );
            Ok((
                [
                    (CONTENT_TYPE, "text/html; charset=utf-8"),
                    (CACHE_CONTROL, "no-cache"),
                ],
                Body::from(contents),
            )
                .into_response())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tracing::info!(
                index_path = %asset.display_path.display(),
                "static index fallback unavailable"
            );
            Ok(StatusCode::NOT_FOUND.into_response())
        }
        Err(error) => {
            tracing::error!(
                index_path = %asset.display_path.display(),
                error_kind = ?error.kind(),
                "failed to read static index fallback"
            );
            Err(AppError::internal("failed to read static index"))
        }
    }
}

/// Serves a concrete static asset from `dist` for legacy frontend routes.
///
/// The route itself decides which URL prefixes are public. This handler only
/// maps the requested URL path to the same relative path under `dist`, validates
/// that the final canonical path still lives below `dist`, and then serves the
/// file. This mirrors the Go panel's static asset behavior while preventing
/// path traversal and symlink escapes.
pub(crate) async fn file_handler(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
) -> AppResult<Response> {
    let request_path = uri.path();
    let Some(asset) = open_dist_asset(&state.root_path, request_path)? else {
        tracing::warn!(path = %request_path, "rejected unsafe static asset path");
        return Ok(StatusCode::NOT_FOUND.into_response());
    };

    let response_body = if method == Method::HEAD {
        Body::empty()
    } else {
        match read_static_asset_body(asset.file) {
            Ok(contents) => Body::from(contents),
            Err(error) => {
                tracing::error!(
                    path = %request_path,
                    asset_path = %asset.display_path.display(),
                    error_kind = ?error.kind(),
                    "failed to read static asset"
                );
                return Err(AppError::internal("failed to read static asset"));
            }
        }
    };

    tracing::debug!(
        method = %method,
        path = %request_path,
        asset_path = %asset.display_path.display(),
        bytes = asset.len,
        "serving static asset"
    );

    let mut response = response_body.into_response();
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static(content_type(&asset.display_path)),
    );
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=30672000"),
    );
    let content_length = HeaderValue::from_str(&asset.len.to_string())
        .map_err(|_| AppError::internal("failed to encode static asset length"))?;
    headers.insert(CONTENT_LENGTH, content_length);
    Ok(response)
}

struct OpenStaticAsset {
    display_path: PathBuf,
    file: fs::File,
    len: u64,
}

fn open_dist_asset(root_path: &Path, request_path: &str) -> AppResult<Option<OpenStaticAsset>> {
    let Some(relative_path) = safe_relative_static_path(request_path) else {
        return Ok(None);
    };

    let dist_root = root_path.join("dist");
    let display_path = dist_root.join(relative_path);
    let file = match safe_open_existing_file_under_base(&dist_root, relative_path) {
        Ok(file) => file,
        Err(error) => {
            tracing::info!(
                path = %request_path,
                asset_path = %display_path.display(),
                error = %error,
                "static asset unavailable or unsafe"
            );
            return Ok(None);
        }
    };
    let metadata = file.metadata().map_err(|error| {
        tracing::error!(
            path = %request_path,
            asset_path = %display_path.display(),
            error_kind = ?error.kind(),
            "failed to read static asset metadata"
        );
        AppError::internal("failed to read static asset metadata")
    })?;

    Ok(Some(OpenStaticAsset {
        display_path,
        file,
        len: metadata.len(),
    }))
}

fn read_static_asset_body(mut file: fs::File) -> io::Result<Vec<u8>> {
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)?;
    Ok(contents)
}

fn safe_relative_static_path(request_path: &str) -> Option<&str> {
    let path_without_leading_slash = request_path.strip_prefix('/')?;
    if path_without_leading_slash.is_empty()
        || contains_encoded_static_path_escape(path_without_leading_slash)
        || path_without_leading_slash.contains('\\')
        || path_without_leading_slash
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == ".." || part.contains(':'))
    {
        return None;
    }
    Some(path_without_leading_slash)
}

fn contains_encoded_static_path_escape(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    ["%00", "%2e", "%2f", "%5c"]
        .iter()
        .any(|pattern| lower.contains(pattern))
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("css") => "text/css; charset=utf-8",
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("ico") => "image/x-icon",
        Some("js") | Some("mjs") => "application/javascript; charset=utf-8",
        Some("json") | Some("map") => "application/json; charset=utf-8",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("svg") => "image/svg+xml",
        Some("webm") => "video/webm",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}
