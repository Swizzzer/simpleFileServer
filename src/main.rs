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
use futures::Stream;
use moka::future::Cache;
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    future::Future,
    net::SocketAddr,
    path::{Path as StdPath, PathBuf},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::SystemTime,
};
use tokio::{
    fs::File,
    time::{Duration, Instant, Sleep},
};
use tokio_util::io::ReaderStream;
use tower_http::cors::CorsLayer;
use tracing::{error, info, warn};
mod log;
mod templates;

const CACHE_FILE_SIZE_LIMIT: u64 = 4 * 1024 * 1024; // 缓存文件大小限制4MB
const CACHE_FILE_NUM_LIMIT: u64 = 128; // 最多缓存128个文件
const RATE_LIMIT_BYTES_PER_SEC: usize = 100 * 1024 * 1024; // 限速100MB/s
const CACHE_FILE_LIFETIME: Duration = Duration::from_secs(2 * 60 * 60); // 缓存文件2小时
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
struct CachedFile {
    data: Arc<Vec<u8>>,
    modified: SystemTime,
}

#[derive(Clone)]
struct AppState {
    root_dir: PathBuf,
    file_cache: Cache<PathBuf, CachedFile>,
}
// 套娃，用于限速
// 避免下行速率过高导致CPU满载
struct RateLimitedStream<S> {
    inner: S,
    bytes_sent: usize,
    window_start: Instant,
    sleep: Option<Pin<Box<Sleep>>>,
}

impl<S> RateLimitedStream<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            bytes_sent: 0,
            window_start: Instant::now(),
            sleep: None,
        }
    }
}

impl<S> Stream for RateLimitedStream<S>
where
    S: Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin,
{
    type Item = Result<bytes::Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let now = Instant::now();
        if now.duration_since(self.window_start) >= Duration::from_secs(1) {
            self.bytes_sent = 0;
            self.window_start = now;
        }

        // 如果有sleep，优先等待
        if let Some(ref mut sleep) = self.sleep {
            match sleep.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(_) => self.sleep = None,
            }
        }

        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                self.bytes_sent += chunk.len();
                if self.bytes_sent > RATE_LIMIT_BYTES_PER_SEC {
                    // 超过速率，延迟到下一秒
                    let delay = self.window_start + Duration::from_secs(1) - now;
                    self.sleep = Some(Box::pin(tokio::time::sleep(delay)));
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    Poll::Ready(Some(Ok(chunk)))
                }
            }
            other => other,
        }
    }
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
        file_cache: Cache::builder()
            .max_capacity(CACHE_FILE_NUM_LIMIT)
            .time_to_live(CACHE_FILE_LIFETIME)
            .build(),
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
    let file_modified = fs::metadata(&file_path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    match file_size <= CACHE_FILE_SIZE_LIMIT && file_size > 0 {
        // 小文件缓存
        true => {
            // 缓存命中
            if let Some(cached) = state.file_cache.get(&file_path).await {
                if cached.modified == file_modified {
                    info!("Serving cached file: {}", file_path.display());
                    return Ok(small_file_response(
                        &file_path,
                        cached.data.clone(),
                        file_size,
                    ));
                } else {
                    info!(
                        "File updated on disk, refreshing cache: {}",
                        file_path.display()
                    );
                }
            }
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
        false => {
            // 大文件流式传输
            info!("Serving large file: {}", file_path.display());
            let file = File::open(&file_path).await.map_err(|e| {
                error!("Failed to open file {}: {}", file_path.display(), e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
            // 计算合适的缓冲区大小
            let buffer_size = match file_size {
                4_194_305..=16_777_216 => 256 * 1024,  // 4MB~16MB: 256KB
                16_777_217..=67_108_928 => 512 * 1024, // 16MB~64MB: 512KB
                67_108_929..=1_073_741_824 => 1 * 1024 * 1024, // 64MB~1GB: 1MB
                _ => 2 * 1024 * 1024,                  // >1GB: 2MB
            };

            let stream = ReaderStream::with_capacity(file, buffer_size);
            // 看起来不是很优雅
            // 也不是不行
            let stream_limited = RateLimitedStream::new(stream);
            let body = axum::body::Body::from_stream(stream_limited);
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
    
    // (file_name, is_dir, size)
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
