use axum::{
    extract::ConnectInfo,
    http::{Method, Uri},
    middleware::Next,
    response::Response,
};
use colored::*;
use std::{net::SocketAddr, time::Instant};
use tracing_subscriber::{fmt, EnvFilter};
use crate::Args;
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .with_env_filter(filter)
        .with_ansi(true)
        .with_target(false)
        .without_time()
        .init();
}

pub async fn logging(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    method: Method,
    uri: Uri,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let start = Instant::now();
    let path = uri.path();
    let query = uri.query().unwrap_or("");

    let response = next.run(request).await;
    let status = response.status();
    let duration = start.elapsed();

    let timestamp = time::OffsetDateTime::now_local()
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc())
        .format(
            &time::format_description::parse(
                "[day]/[month repr:short]/[year] [hour]:[minute]:[second]",
            )
            .unwrap(),
        )
        .unwrap_or_else(|_| "Unknown".to_string());

    let full_path = if query.is_empty() {
        path.to_string()
    } else {
        format!("{}?{}", path, query)
    };

    let status_colored = match status.as_u16() {
        200..=299 => status.to_string().green(),
        300..=399 => status.to_string().yellow(),
        400..=499 => status.to_string().red(),
        500..=599 => status.to_string().bright_red(),
        _ => status.to_string().normal(),
    };

    let method_colored = match method.as_str() {
        "GET" => method.to_string().blue(),
        "POST" => method.to_string().green(),
        "PUT" => method.to_string().yellow(),
        "DELETE" => method.to_string().red(),
        _ => method.to_string().normal(),
    };

    println!(
        "{} - - [{}] \"{} {} HTTP/1.1\" {} {} - {:.2}ms",
        addr.ip().to_string().cyan(),
        timestamp,
        method_colored,
        full_path.bright_white(),
        status_colored,
        response
            .headers()
            .get("content-length")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("-"),
        duration.as_millis()
    );

    // 错误状态码额外打印错误信息
    if status.is_client_error() || status.is_server_error() {
        match status.as_u16() {
            404 => println!("  └─ {} File not found: {}", "ERROR".bright_red(), path),
            403 => println!("  └─ {} Access forbidden: {}", "ERROR".bright_red(), path),
            500 => println!(
                "  └─ {} Internal server error: {}",
                "ERROR".bright_red(),
                path
            ),
            _ => {}
        }
    }

    response
}
pub fn banner(args: &Args, serve_dir: &std::path::Path) {
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
