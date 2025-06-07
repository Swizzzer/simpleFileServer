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
    time::SystemTime,
};
use tokio::{fs::File, time::Duration};
use tokio_util::io::ReaderStream;
use tower_http::cors::CorsLayer;
use tracing::{error, info, warn};
mod log;
mod rate_limiter;
mod sliding_window;
mod templates;
use rate_limiter::RateLimiterFactory;

const CACHE_FILE_SIZE_LIMIT: u64 = 4 * 1024 * 1024; // 缓存文件大小限制4MB
const CACHE_FILE_NUM_LIMIT: u64 = 128; // 最多缓存128个文件
const RATE_LIMIT_BYTES_PER_SEC: usize = 100 * 1024 * 1024; // 限速100MB/s
const CACHE_FILE_LIFETIME: Duration = Duration::from_secs(2 * 60 * 60); // 缓存文件2小时
const SLIDING_WINDOW_THRESHOLD: u64 = 16 * 1024 * 1024; // 16MB以上的文件使用滑动窗口

#[derive(Parser)]
#[command(name = "http-file-server")]
#[command(about = "A simple HTTP file server with sliding window optimization")]
struct Args {
    #[arg(short, long, default_value = "8000")]
    port: u16,

    #[arg(short, long, default_value = "0.0.0.0")]
    bind: String,

    #[arg(help = "Directory to serve (default: current directory)")]
    directory: Option<PathBuf>,

    #[arg(long, help = "Disable sliding window optimization for large files")]
    disable_sliding_window: bool,
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
struct CachedFile {
    data: Arc<Vec<u8>>,
    modified: SystemTime,
}

#[derive(Clone)]
struct AppState {
    root_dir: PathBuf,
    file_cache: Cache<PathBuf, CachedFile>,
    sliding_window_enabled: bool,
    rate_limiter_factory: RateLimiterFactory,
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

    if !args.disable_sliding_window {
        sliding_window::init_session_manager().await;
        info!(
            "Sliding window optimization enabled for files > {}MB",
            SLIDING_WINDOW_THRESHOLD / 1024 / 1024
        );
    } else {
        info!("Sliding window optimization disabled");
    }

    let rate_limiter_factory = RateLimiterFactory::new(RATE_LIMIT_BYTES_PER_SEC).with_burst(0.2); // 20%突发
    let app_state = AppState {
        root_dir: serve_dir,
        file_cache: Cache::builder()
            .max_capacity(CACHE_FILE_NUM_LIMIT)
            .time_to_live(CACHE_FILE_LIFETIME)
            .build(),
        sliding_window_enabled: !args.disable_sliding_window,
        rate_limiter_factory,
    };

    let app = Router::new()
        .route("/", get(handle_directory))
        .route("/*path", get(handle_path))
        .layer(middleware::from_fn(log::logging))
        .layer(CorsLayer::permissive())
        .with_state(app_state);

    let addr = format!("{}:{}", args.bind, args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    let optimization_status = if args.disable_sliding_window {
        "standard mode"
    } else {
        "with sliding window optimization"
    };

    println!(
        "{} Server ready at {} ({})",
        "✓".green(),
        format!("http://{}", addr).bright_blue().underline(),
        optimization_status.cyan()
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
    let file_modified = fs::metadata(&file_path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);

    match file_size {
        size if size <= CACHE_FILE_SIZE_LIMIT && size > 0 => {
            info!(
                "Serving small file (cached): {} ({} bytes)",
                file_path.display(),
                size
            );
            serve_small_file(file_path, state, file_size, file_modified).await
        }
        size if size <= SLIDING_WINDOW_THRESHOLD => {
            info!(
                "Serving medium file (streaming): {} ({} bytes)",
                file_path.display(),
                size
            );
            serve_medium_file(file_path, file_size, &state.rate_limiter_factory).await
        }
        size => {
            if state.sliding_window_enabled {
                info!(
                    "Serving large file (sliding window): {} ({} bytes)",
                    file_path.display(),
                    size
                );
                serve_large_file_with_sliding_window(
                    file_path,
                    file_size,
                    &state.rate_limiter_factory,
                )
                .await
            } else {
                info!(
                    "Serving large file (streaming): {} ({} bytes)",
                    file_path.display(),
                    size
                );
                serve_medium_file(file_path, file_size, &state.rate_limiter_factory).await
            }
        }
    }
}

async fn serve_small_file(
    file_path: PathBuf,
    state: &AppState,
    file_size: u64,
    file_modified: SystemTime,
) -> Result<Response, StatusCode> {
    // 缓存命中
    if let Some(cached) = state.file_cache.get(&file_path).await {
        if cached.modified == file_modified {
            info!("Cache hit for small file: {}", file_path.display());
            return Ok(small_file_response(
                &file_path,
                cached.data.clone(),
                file_size,
            ));
        } else {
            info!("File updated, refreshing cache: {}", file_path.display());
        }
    }
    // 缓存未命中
    let data = tokio::fs::read(&file_path).await.map_err(|e| {
        error!("Failed to read file {}: {}", file_path.display(), e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let arc_data = Arc::new(data);
    let cached = CachedFile {
        data: arc_data.clone(),
        modified: file_modified,
    };
    state.file_cache.insert(file_path.clone(), cached).await;
    info!("Small file cached: {}", file_path.display());

    Ok(small_file_response(&file_path, arc_data, file_size))
}

async fn serve_medium_file(
    file_path: PathBuf,
    file_size: u64,
    rate_limiter_factory: &RateLimiterFactory,
) -> Result<Response, StatusCode> {
    let file = File::open(&file_path).await.map_err(|e| {
        error!("Failed to open file {}: {}", file_path.display(), e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let buffer_size = calculate_buffer_size(file_size);
    let stream = ReaderStream::with_capacity(file, buffer_size);
    let stream_limited = rate_limiter_factory.create_stream(stream, None);
    let body = axum::body::Body::from_stream(stream_limited);
    let headers = build_headers(&file_path, file_size);
    Ok((headers, body).into_response())
}

async fn serve_large_file_with_sliding_window(
    file_path: PathBuf,
    file_size: u64,
    rate_limiter_factory: &RateLimiterFactory,
) -> Result<Response, StatusCode> {
    let buffer_size = calculate_buffer_size(file_size);

    let rate_limiter = rate_limiter_factory.create_limiter(None);

    let sliding_window_stream = sliding_window::SlidingWindowFileStream::new(
        file_path.clone(),
        buffer_size,
        Some(rate_limiter),
    )
    .await
    .map_err(|e| {
        error!(
            "Failed to create sliding window stream for {}: {}",
            file_path.display(),
            e
        );
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let body = axum::body::Body::from_stream(sliding_window_stream);
    let headers = build_headers(&file_path, file_size);
    Ok((headers, body).into_response())
}

// 计算缓冲区大小
fn calculate_buffer_size(file_size: u64) -> usize {
    match file_size {
        0..=4_194_304 => 64 * 1024,                     // 0-4MB: 64KB
        4_194_305..=16_777_216 => 256 * 1024,           // 4MB-16MB: 256KB
        16_777_217..=67_108_864 => 512 * 1024,          // 16MB-64MB: 512KB
        67_108_865..=268_435_456 => 1024 * 1024,        // 64MB-256MB: 1MB
        268_435_457..=1_073_741_824 => 2 * 1024 * 1024, // 256MB-1GB: 2MB
        _ => 4 * 1024 * 1024,                           // >1GB: 4MB
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

    // 添加缓存控制头
    headers.insert(
        header::CACHE_CONTROL,
        "public, max-age=3600".parse().unwrap(),
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
        .map(|res| {
            res.map_err(|e| {
                error!("Failed to read entry: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })
            .and_then(|entry| {
                let file_name = entry.file_name();
                let metadata = entry.metadata().map_err(|e| {
                    error!("Failed to read metadata: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;
                let is_dir = metadata.is_dir();
                let size = if is_dir { None } else { Some(metadata.len()) };
                Ok((file_name, is_dir, size))
            })
        })
        .collect::<Result<Vec<_>, StatusCode>>()?;

    // 排序：目录优先，然后按名称排序
    dir_entries.sort_by(|a, b| match (a.1, b.1) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.0.cmp(&b.0),
    });

    for (file_name, is_dir, size) in dir_entries {
        let file_name_str = file_name.to_string_lossy().to_string();
        let entry_path = if current_path.is_empty() {
            file_name_str.clone()
        } else {
            format!("{}/{}", current_path.trim_end_matches('/'), file_name_str)
        };
        let encoded_path = utf8_percent_encode(&entry_path, NON_ALPHANUMERIC).to_string();

        entries.push(FileEntry {
            name: file_name_str,
            is_dir,
            size,
            url: format!("/{}", encoded_path),
        });
    }

    let html = templates::generate_html(&entries, current_path);
    Ok(Html(html).into_response())
}
