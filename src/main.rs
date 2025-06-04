use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use clap::Parser;
use colored::*;
use moka::future::Cache;
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    net::SocketAddr,
    path::{Path as StdPath, PathBuf},
    sync::Arc,
};
use tokio::fs::File;
use tokio_util::io::ReaderStream;
use tower_http::cors::CorsLayer;
use tracing::{error, info, warn};
mod log;
mod templates;
const CACHE_FILE_SIZE_LIMIT: u64 = 4 * 1024 * 1024; // 4MB
const CACHE_FILE_NUM_LIMIT: u64 = 128; // 最多缓存128个文件

#[derive(Parser)]
#[command(name = "http-file-server")]
#[command(about = "A simple HTTP file server similar to `python -m http.server`")]
struct Args {
    #[arg(short, long, default_value = "8000")]
    port: u16,

    #[arg(short, long, default_value = "0.0.0.0")]
    bind: String,

    #[arg(help = "Directory to serve (default: current directory)")]
    directory: Option<PathBuf>,
}

#[derive(Serialize)]
struct FileEntry {
    name: String,
    is_dir: bool,
    size: Option<u64>,
    url: String,
}

#[derive(Deserialize)]
struct DownloadQuery {
    download: Option<String>,
}

#[derive(Clone)]
struct AppState {
    root_dir: PathBuf,
    file_cache: Cache<PathBuf, Arc<Vec<u8>>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    log::init();
    let serve_dir = args
        .directory
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap());
    let serve_dir = serve_dir.canonicalize()?;

    log::banner(&args, &serve_dir);

    let app_state = AppState {
        root_dir: serve_dir,
        file_cache: Cache::builder().max_capacity(CACHE_FILE_NUM_LIMIT).build(),
    };

    let app = Router::new()
        .route("/", get(handle_directory))
        .route("/*path", get(handle_path))
        .layer(middleware::from_fn(log::logging))
        .layer(CorsLayer::permissive())
        .with_state(app_state);

    let addr = format!("{}:{}", args.bind, args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    println!(
        "{} Server ready at {}",
        "✓".green(),
        format!("http://{}", addr).bright_blue().underline()
    );
    println!("{} Press Ctrl+C to stop", "ⓘ".blue());
    println!();

    let result = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await;

    if let Err(e) = result {
        error!("Server error: {}", e);
    }

    Ok(())
}

async fn handle_directory(
    State(state): State<AppState>,
    Query(params): Query<DownloadQuery>,
) -> Result<Response, StatusCode> {
    handle_path_internal(state, "".to_string(), params).await
}

async fn handle_path(
    State(state): State<AppState>,
    Path(path): Path<String>,
    Query(params): Query<DownloadQuery>,
) -> Result<Response, StatusCode> {
    handle_path_internal(state, path, params).await
}

async fn handle_path_internal(
    state: AppState,
    path: String,
    params: DownloadQuery,
) -> Result<Response, StatusCode> {
    let decoded_path = percent_decode_str(&path).decode_utf8().map_err(|_| {
        warn!("Invalid UTF-8 in path: {}", path);
        StatusCode::BAD_REQUEST
    })?;

    // 防止目录穿越
    let requested_path = state.root_dir.join(&*decoded_path);
    let canonical_path = requested_path.canonicalize().map_err(|_| {
        warn!("Path not found: {}", decoded_path);
        StatusCode::NOT_FOUND
    })?;
    if !canonical_path.starts_with(&state.root_dir) {
        warn!("Directory traversal attempt blocked: {}", decoded_path);
        return Err(StatusCode::FORBIDDEN);
    }

    let metadata = fs::metadata(&canonical_path).map_err(|_| {
        warn!("Cannot read metadata for: {}", canonical_path.display());
        StatusCode::NOT_FOUND
    })?;

    if metadata.is_file() {
        if params.download.is_some() || !metadata.is_dir() {
            info!("Serving file: {}", canonical_path.display());
            return serve_file(canonical_path, &state).await;
        }
    }

    if metadata.is_dir() {
        info!("Serving directory: {}", canonical_path.display());
        return serve_directory(canonical_path, &state.root_dir, &decoded_path).await;
    }

    Err(StatusCode::NOT_FOUND)
}

async fn serve_file(file_path: PathBuf, state: &AppState) -> Result<Response, StatusCode> {
    let file_size = fs::metadata(&file_path).map(|m| m.len()).unwrap_or(0);

    match file_size <= CACHE_FILE_SIZE_LIMIT && file_size > 0 {
        // 小文件缓存
        true => {
            // 缓存命中
            if let Some(data) = state.file_cache.get(&file_path).await {
                info!("Serving cached file: {}", file_path.display());
                return Ok(small_file_response(&file_path, data.clone(), file_size));
            }

            let data = tokio::fs::read(&file_path).await.map_err(|e| {
                error!("Failed to read file {}: {}", file_path.display(), e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
            let arc_data = Arc::new(data);
            state
                .file_cache
                .insert(file_path.clone(), arc_data.clone())
                .await;
            info!("Small file cached: {}", file_path.display());

            Ok(small_file_response(&file_path, arc_data, file_size))
        }
        false => {
            // 大文件流式传输
            info!("Serving large file: {}", file_path.display());
            let file = File::open(&file_path).await.map_err(|e| {
                error!("Failed to open file {}: {}", file_path.display(), e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

            let stream = ReaderStream::new(file);
            let body = axum::body::Body::from_stream(stream);

            let headers = build_headers(&file_path, file_size);

            Ok((headers, body).into_response())
        }
    }
}

fn small_file_response(file_path: &PathBuf, data: Arc<Vec<u8>>, file_size: u64) -> Response {
    let headers = build_headers(file_path, file_size);
    let body = axum::body::Body::from(data.as_ref().clone());
    (headers, body).into_response()
}

fn build_headers(file_path: &PathBuf, file_size: u64) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let content_type = mime_guess::from_path(&file_path)
        .first_or_octet_stream()
        .to_string();
    let file_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("download");
    headers.insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    headers.insert(
        header::CONTENT_LENGTH,
        file_size.to_string().parse().unwrap(),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{}\"", file_name)
            .parse()
            .unwrap(),
    );
    headers
}

async fn serve_directory(
    dir_path: PathBuf,
    root_dir: &StdPath,
    current_path: &str,
) -> Result<Response, StatusCode> {
    let mut entries = Vec::new();

    if dir_path != *root_dir {
        let parent_path = if current_path.is_empty() {
            String::new()
        } else {
            let parts: Vec<&str> = current_path.trim_end_matches('/').split('/').collect();
            if parts.len() > 1 {
                parts[..parts.len() - 1].join("/")
            } else {
                String::new()
            }
        };

        entries.push(FileEntry {
            name: "..".to_string(),
            is_dir: true,
            size: None,
            url: format!("/{}", parent_path),
        });
    }

    let mut dir_entries = fs::read_dir(&dir_path)
        .map_err(|e| {
            error!("Failed to read directory {}: {}", dir_path.display(), e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            error!("Error reading directory entries: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // 排序：目录在前，文件在后，按名称排序
    dir_entries.sort_by(|a, b| {
        let a_is_dir = a.path().is_dir();
        let b_is_dir = b.path().is_dir();

        match (a_is_dir, b_is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.file_name().cmp(&b.file_name()),
        }
    });

    for entry in dir_entries {
        let file_name = entry.file_name().to_string_lossy().to_string();
        let metadata = entry
            .metadata()
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let is_dir = metadata.is_dir();

        let entry_path = if current_path.is_empty() {
            file_name.clone()
        } else {
            format!("{}/{}", current_path.trim_end_matches('/'), file_name)
        };

        let encoded_path = utf8_percent_encode(&entry_path, NON_ALPHANUMERIC).to_string();

        entries.push(FileEntry {
            name: file_name,
            is_dir,
            size: if is_dir { None } else { Some(metadata.len()) },
            url: format!("/{}", encoded_path),
        });
    }

    let html = templates::generate_html(&entries, current_path);
    Ok(Html(html).into_response())
}
