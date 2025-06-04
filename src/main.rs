use axum::{
    extract::{Path, Query},
    http::{header, HeaderMap, StatusCode},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use clap::Parser;
use colored::*;
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    net::SocketAddr,
    path::{Path as StdPath, PathBuf},
};
use tokio::fs::File;
use tokio_util::io::ReaderStream;
use tower_http::cors::CorsLayer;
use tracing::{error, info, warn};
mod log;

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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    log::init();
    // 用clone()避免所有权转移
    // TODO: 是否有更优雅的解决方案?
    let serve_dir = args
        .directory
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap());
    let serve_dir = serve_dir.canonicalize()?;

    banner(&args, &serve_dir);

    let app_state = AppState {
        root_dir: serve_dir,
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
    axum::extract::State(state): axum::extract::State<AppState>,
    Query(params): Query<DownloadQuery>,
) -> Result<Response, StatusCode> {
    handle_path_internal(state, "".to_string(), params).await
}

async fn handle_path(
    axum::extract::State(state): axum::extract::State<AppState>,
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
            return serve_file(canonical_path).await;
        }
    }

    if metadata.is_dir() {
        info!("Serving directory: {}", canonical_path.display());
        return serve_directory(canonical_path, &state.root_dir, &decoded_path).await;
    }

    Err(StatusCode::NOT_FOUND)
}

async fn serve_file(file_path: PathBuf) -> Result<Response, StatusCode> {
    let file = File::open(&file_path).await.map_err(|e| {
        error!("Failed to open file {}: {}", file_path.display(), e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let stream = ReaderStream::new(file);
    let body = axum::body::Body::from_stream(stream);

    let content_type = mime_guess::from_path(&file_path)
        .first_or_octet_stream()
        .to_string();

    let file_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("download");

    let file_size = fs::metadata(&file_path).map(|m| m.len()).unwrap_or(0);

    let mut headers = HeaderMap::new();
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

    Ok((headers, body).into_response())
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

    let html = generate_html(&entries, current_path);
    Ok(Html(html).into_response())
}

fn banner(args: &Args, serve_dir: &std::path::Path) {
    println!();
    println!(
        "{}",
        "═══════════════════════════════════════════════════════".bright_blue()
    );
    println!(
        "{} {}",
        "🚀 Swizzer's HTTP File Server".bright_white().bold(),
        "v0.1.0".bright_black()
    );
    println!(
        "{}",
        "═══════════════════════════════════════════════════════".bright_blue()
    );
    println!();
    println!(
        "{:<15} {}",
        "Serving:".bright_white(),
        serve_dir.display().to_string().cyan()
    );
    println!(
        "{:<15} {}:{}",
        "Binding:".bright_white(),
        args.bind.yellow(),
        args.port.to_string().yellow()
    );
    println!(
        "{:<15} {}",
        "Started at:".bright_white(),
        time::OffsetDateTime::now_local()
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc())
            .format(
                &time::format_description::parse("[year]-[month]-[day] [hour]:[minute]:[second]")
                    .unwrap()
            )
            .unwrap_or_else(|_| "Unknown".to_string())
            .bright_green()
    );
    println!();
}

fn generate_html(entries: &[FileEntry], current_path: &str) -> String {
    let entries_json = serde_json::to_string(entries).unwrap_or_else(|_| "[]".to_string());
    let current_path_display = if current_path.is_empty() {
        "/"
    } else {
        current_path
    };

    format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>😊 Swizzer's Sharing Service - {}</title>
    <link href="https://fonts.googleapis.com/css2?family=Inter:wght@300;400;500;600&display=swap" rel="stylesheet">
    <link href="https://fonts.googleapis.com/icon?family=Material+Icons" rel="stylesheet">
    <style>
        * {{
            margin: 0;
            padding: 0;
            box-sizing: border-box;
        }}
        
        body {{
            font-family: 'Inter', -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
            min-height: 100vh;
            color: #333;
            overflow-x: hidden;
        }}
        
        .container {{
            max-width: 1200px;
            margin: 0 auto;
            padding: 2rem;
        }}
        
        .header {{
            background: rgba(255, 255, 255, 0.95);
            backdrop-filter: blur(20px);
            border-radius: 16px;
            padding: 2rem;
            margin-bottom: 2rem;
            box-shadow: 0 8px 32px rgba(0, 0, 0, 0.1);
            border: 1px solid rgba(255, 255, 255, 0.2);
        }}
        
        .header h1 {{
            font-size: 2.5rem;
            font-weight: 600;
            background: linear-gradient(135deg, #667eea, #764ba2);
            -webkit-background-clip: text;
            -webkit-text-fill-color: transparent;
            margin-bottom: 0.5rem;
        }}
        
        .breadcrumb {{
            display: flex;
            align-items: center;
            font-size: 1rem;
            color: #666;
            gap: 0.5rem;
        }}
        
        .breadcrumb .material-icons {{
            font-size: 1.2rem;
            color: #888;
        }}
        
        .file-grid {{
            background: rgba(255, 255, 255, 0.95);
            backdrop-filter: blur(20px);
            border-radius: 16px;
            padding: 2rem;
            box-shadow: 0 8px 32px rgba(0, 0, 0, 0.1);
            border: 1px solid rgba(255, 255, 255, 0.2);
        }}
        
        .file-list {{
            display: grid;
            gap: 0.5rem;
        }}
        
        .file-item {{
            display: flex;
            align-items: center;
            padding: 1rem 1.5rem;
            border-radius: 12px;
            text-decoration: none;
            color: inherit;
            transition: all 0.3s cubic-bezier(0.4, 0, 0.2, 1);
            border: 1px solid transparent;
            background: rgba(255, 255, 255, 0.7);
            position: relative;
            overflow: hidden;
        }}
        
        .file-item::before {{
            content: '';
            position: absolute;
            top: 0;
            left: -100%;
            width: 100%;
            height: 100%;
            background: linear-gradient(90deg, transparent, rgba(255, 255, 255, 0.4), transparent);
            transition: left 0.5s;
        }}
        
        .file-item:hover {{
            transform: translateY(-2px);
            box-shadow: 0 8px 25px rgba(0, 0, 0, 0.15);
            border-color: rgba(102, 126, 234, 0.3);
            background: rgba(255, 255, 255, 0.9);
        }}
        
        .file-item:hover::before {{
            left: 100%;
        }}
        
        .file-icon {{
            margin-right: 1rem;
            font-size: 1.5rem;
            width: 2rem;
            text-align: center;
            color: #667eea;
        }}
        
        .file-info {{
            flex: 1;
            display: flex;
            justify-content: space-between;
            align-items: center;
        }}
        
        .file-name {{
            font-weight: 500;
            font-size: 1rem;
            color: #333;
        }}
        
        .file-size {{
            font-size: 0.875rem;
            color: #888;
            font-weight: 400;
        }}
        
        .download-btn {{
            margin-left: 1rem;
            padding: 0.5rem;
            border: none;
            background: linear-gradient(135deg, #667eea, #764ba2);
            color: white;
            border-radius: 8px;
            cursor: pointer;
            transition: all 0.3s ease;
            opacity: 0;
            transform: translateX(10px);
        }}
        
        .file-item:hover .download-btn {{
            opacity: 1;
            transform: translateX(0);
        }}
        
        .download-btn:hover {{
            transform: scale(1.1);
            box-shadow: 0 4px 15px rgba(102, 126, 234, 0.4);
        }}
        
        .download-btn .material-icons {{
            font-size: 1.2rem;
        }}
        
        .empty-state {{
            text-align: center;
            padding: 4rem 2rem;
            color: #888;
        }}
        
        .empty-state .material-icons {{
            font-size: 4rem;
            margin-bottom: 1rem;
            opacity: 0.5;
        }}
        
        @media (max-width: 768px) {{
            .container {{
                padding: 1rem;
            }}
            
            .header {{
                padding: 1.5rem;
            }}
            
            .header h1 {{
                font-size: 2rem;
            }}
            
            .file-grid {{
                padding: 1rem;
            }}
            
            .file-item {{
                padding: 1rem;
            }}
            
            .file-info {{
                flex-direction: column;
                align-items: flex-start;
                gap: 0.5rem;
            }}
            
            .download-btn {{
                position: absolute;
                right: 1rem;
                top: 50%;
                transform: translateY(-50%);
                opacity: 1;
            }}
        }}
        
        .parent-dir {{
            background: linear-gradient(135deg, rgba(102, 126, 234, 0.1), rgba(118, 75, 162, 0.1));
            border: 1px solid rgba(102, 126, 234, 0.2);
        }}
        
        .parent-dir .file-icon {{
            color: #764ba2;
        }}
        
        .fade-in {{
            animation: fadeIn 0.6s ease-out;
        }}
        
        @keyframes fadeIn {{
            from {{
                opacity: 0;
                transform: translateY(20px);
            }}
            to {{
                opacity: 1;
                transform: translateY(0);
            }}
        }}
    </style>
</head>
<body>
    <div class="container">
        <div class="header fade-in">
            <h1>Swizzer's Sharing Service</h1>
            <div class="breadcrumb">
                <span class="material-icons">folder</span>
                <span id="currentPath">{}</span>
            </div>
        </div>
        
        <div class="file-grid fade-in">
            <div class="file-list" id="fileList">
                <!-- 文件列表将通过JavaScript生成 -->
            </div>
        </div>
    </div>
    
    <script>
        const entries = {};
        
        function formatFileSize(bytes) {{
            if (bytes === null || bytes === undefined) return '';
            const sizes = ['B', 'KB', 'MB', 'GB'];
            if (bytes === 0) return '0 B';
            const i = Math.floor(Math.log(bytes) / Math.log(1024));
            return Math.round(bytes / Math.pow(1024, i) * 100) / 100 + ' ' + sizes[i];
        }}
        
        function getFileIcon(fileName, isDir) {{
            if (fileName === '..') return 'keyboard_arrow_up';
            if (isDir) return 'folder';
            
            const ext = fileName.split('.').pop().toLowerCase();
            const iconMap = {{
                'pdf': 'picture_as_pdf',
                'doc': 'description',
                'docx': 'description',
                'xls': 'table_chart',
                'xlsx': 'table_chart',
                'ppt': 'slideshow',
                'pptx': 'slideshow',
                'txt': 'text_snippet',
                'md': 'text_snippet',
                'zip': 'archive',
                'rar': 'archive',
                '7z': 'archive',
                'jpg': 'image',
                'jpeg': 'image',
                'png': 'image',
                'gif': 'image',
                'svg': 'image',
                'mp4': 'movie',
                'avi': 'movie',
                'mkv': 'movie',
                'mp3': 'audiotrack',
                'wav': 'audiotrack',
                'flac': 'audiotrack',
                'js': 'code',
                'html': 'code',
                'css': 'code',
                'json': 'code',
                'xml': 'code',
                'py': 'code',
                'java': 'code',
                'cpp': 'code',
                'c': 'code',
                'rs': 'code'
            }};
            
            return iconMap[ext] || 'insert_drive_file';
        }}
        
        function renderFileList() {{
            const fileList = document.getElementById('fileList');
            
            if (entries.length === 0) {{
                fileList.innerHTML = `
                    <div class="empty-state">
                        <div class="material-icons">folder_open</div>
                        <p>此目录为空</p>
                    </div>
                `;
                return;
            }}
            
            fileList.innerHTML = entries.map((entry, index) => {{
                const icon = getFileIcon(entry.name, entry.is_dir);
                const sizeDisplay = entry.is_dir ? '' : formatFileSize(entry.size);
                const isParentDir = entry.name === '..';
                const itemClass = isParentDir ? 'file-item parent-dir' : 'file-item';
                
                const downloadBtn = !entry.is_dir ? `
                    <button class="download-btn" onclick="downloadFile('${{entry.url}}', event)" title="下载文件">
                        <span class="material-icons">download</span>
                    </button>
                ` : '';
                
                return `
                    <a href="${{entry.url}}" class="${{itemClass}}" style="animation-delay: ${{index * 0.1}}s">
                        <span class="material-icons file-icon">${{icon}}</span>
                        <div class="file-info">
                            <span class="file-name">${{entry.name}}</span>
                            <span class="file-size">${{sizeDisplay}}</span>
                        </div>
                        ${{downloadBtn}}
                    </a>
                `;
            }}).join('');
        }}
        
        function downloadFile(url, event) {{
            event.preventDefault();
            event.stopPropagation();
            window.location.href = url + '?download=1';
        }}
        
        document.addEventListener('DOMContentLoaded', () => {{
            renderFileList();
        }});
    </script>
</body>
</html>"#,
        current_path_display, current_path_display, entries_json
    )
}
