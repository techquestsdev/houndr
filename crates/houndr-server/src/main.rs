//! HTTP server and web UI for houndr code search.
//!
//! Embeds the Axum web server, background indexing watcher, and serves
//! the single-page search UI with SSE streaming results.

#![warn(missing_docs)]

mod api;
mod state;
mod ui;

use axum::extract::ConnectInfo;
use axum::extract::DefaultBodyLimit;
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::{routing::get, Extension, Router};
use clap::Parser;
use governor::clock::DefaultClock;
use governor::state::keyed::DashMapStateStore;
use governor::{Quota, RateLimiter};
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use tracing::info;

type KeyedLimiter = RateLimiter<IpAddr, DashMapStateStore<IpAddr>, DefaultClock>;

use crate::state::AppState;
use houndr_repo::config::Config;

#[derive(Parser)]
#[command(name = "houndr", about = "Code search engine")]
struct Cli {
    /// Path to config file
    #[arg(short, long, default_value = "config.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "houndr_server=info,houndr_repo=info,houndr_index=info".into());

    let log_format = std::env::var("LOG_FORMAT").unwrap_or_default();
    if log_format.eq_ignore_ascii_case("json") {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(env_filter)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }

    let cli = Cli::parse();
    let config = Config::from_file(&cli.config)?;
    let bind_addr = config.server.bind.clone();

    let app_state = Arc::new(AppState::new(config.clone()));

    // Pre-load existing indexes for instant search availability
    {
        use crate::state::RepoStatus;
        let data_dir = std::path::Path::new(&config.indexer.data_dir);
        let mut preloaded = Vec::new();
        let mut statuses = app_state.repo_statuses.write().await;
        for repo in &config.repos {
            if let Some(reader) = houndr_repo::pipeline::load_existing_index(&repo.name, data_dir) {
                info!(repo = %repo.name, "pre-loaded existing index");
                statuses.insert(repo.name.clone(), RepoStatus::Ready);
                preloaded.push(reader);
            }
        }
        drop(statuses);
        if !preloaded.is_empty() {
            let count = preloaded.len();
            let mut readers = app_state.readers.write().await;
            *readers = preloaded;
            info!(count, "pre-loaded indexes ready for search");
        }
    }

    // Start the watcher in a background task
    let watcher_config = config.clone();
    let watcher_state = app_state.clone();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let watcher_shutdown = shutdown.clone();
    let mut watcher_handle = tokio::spawn(async move {
        let data_dir = std::path::PathBuf::from(&watcher_config.indexer.data_dir);
        let poll_interval = Duration::from_secs(watcher_config.indexer.poll_interval_secs);
        let max_file_size = watcher_config.indexer.max_file_size;
        let max_concurrent = watcher_config.indexer.max_concurrent_indexers;
        let index_timeout_secs = watcher_config.indexer.index_timeout_secs;

        let exclude_patterns: Vec<glob::Pattern> = watcher_config
            .indexer
            .exclude_patterns
            .iter()
            .filter_map(|p| match glob::Pattern::new(p) {
                Ok(pat) => Some(pat),
                Err(e) => {
                    tracing::warn!(pattern = %p, error = %e, "invalid exclude pattern, skipping");
                    None
                }
            })
            .collect();

        // Initial indexing (cancellation-aware)
        info!(
            "starting initial indexing of {} repos",
            watcher_config.repos.len()
        );
        tokio::select! {
            _ = run_indexing(
                &watcher_config,
                &data_dir,
                max_file_size,
                max_concurrent,
                index_timeout_secs,
                &exclude_patterns,
                &watcher_state,
                true,
                &watcher_shutdown,
            ) => {}
            _ = watcher_shutdown.cancelled() => {
                info!("watcher shutting down during initial indexing");
                return;
            }
        }

        // Polling loop — exits on cancellation
        loop {
            tokio::select! {
                _ = tokio::time::sleep(poll_interval) => {
                    info!("polling repos for changes");
                    tokio::select! {
                        _ = run_indexing(&watcher_config, &data_dir, max_file_size, max_concurrent, index_timeout_secs, &exclude_patterns, &watcher_state, false, &watcher_shutdown) => {}
                        _ = watcher_shutdown.cancelled() => {
                            info!("watcher shutting down");
                            break;
                        }
                    }
                }
                _ = watcher_shutdown.cancelled() => {
                    info!("watcher shutting down");
                    break;
                }
            }
        }
    });

    let rate_limiter: Option<Arc<KeyedLimiter>> = if config.server.rate_limit_rps > 0 {
        let quota =
            Quota::per_second(NonZeroU32::new(config.server.rate_limit_rps as u32).unwrap());
        info!(rps = config.server.rate_limit_rps, "rate limiting enabled");
        Some(Arc::new(RateLimiter::keyed(quota)))
    } else {
        info!("rate limiting disabled (rate_limit_rps = 0)");
        None
    };

    if let Some(ref limiter) = rate_limiter {
        let limiter = limiter.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                limiter.retain_recent();
            }
        });
    }

    let cors = if config.server.cors_origins.is_empty() {
        tracing::warn!("CORS is permissive (no cors_origins configured) — all origins allowed");
        CorsLayer::permissive()
    } else {
        let origins: Vec<HeaderValue> = config
            .server
            .cors_origins
            .iter()
            .map(|o| {
                o.parse::<HeaderValue>()
                    .map_err(|_| anyhow::anyhow!("invalid CORS origin '{}'", o))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods(Any)
            .allow_headers(Any)
    };

    let timeout = Duration::from_secs(config.server.timeout_secs);

    let mut app = Router::new()
        .route("/", get(ui::index_handler))
        .route("/static/app.css", get(ui::css_handler))
        .route("/static/app.js", get(ui::js_handler))
        .route("/static/favicon.svg", get(ui::favicon_handler))
        .route("/api/v1/search", get(api::search_handler))
        .route("/api/v1/search/stream", get(api::search_stream_handler))
        .route("/api/v1/repos", get(api::repos_handler))
        .route("/api/v1/status", get(api::status_handler))
        .route("/healthz", get(api::healthz_handler))
        .with_state(app_state)
        .layer(axum::middleware::from_fn(security_headers));

    if let Some(limiter) = rate_limiter {
        app = app
            .layer(axum::middleware::from_fn(rate_limit_middleware))
            .layer(Extension(limiter));
    }

    let app = app
        .layer(cors)
        .layer(DefaultBodyLimit::max(config.server.max_request_bytes))
        .layer(CompressionLayer::new())
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::GATEWAY_TIMEOUT,
            timeout,
        ))
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    info!("listening on {}", bind_addr);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    // Stop the watcher — cancel first, then abort if it doesn't finish quickly
    info!("stopping watcher...");
    shutdown.cancel();
    match tokio::time::timeout(Duration::from_secs(5), &mut watcher_handle).await {
        Ok(_) => info!("watcher stopped gracefully"),
        Err(_) => {
            info!("watcher still busy, aborting");
            watcher_handle.abort();
            let _ = watcher_handle.await;
        }
    }
    info!("clean shutdown complete");

    Ok(())
}

async fn security_headers(req: axum::extract::Request, next: Next) -> axum::response::Response {
    let is_static = req.uri().path().starts_with("/static/");
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert("x-content-type-options", "nosniff".parse().unwrap());
    h.insert("x-frame-options", "DENY".parse().unwrap());
    h.insert(
        "referrer-policy",
        "strict-origin-when-cross-origin".parse().unwrap(),
    );
    h.insert("x-xss-protection", "0".parse().unwrap());
    // Only apply CSP to HTML pages, not static assets (SVG uses inline presentation attributes)
    if !is_static {
        h.insert(
            "content-security-policy",
            "default-src 'self'; script-src 'self'; style-src 'self'"
                .parse()
                .unwrap(),
        );
    }
    res
}

async fn rate_limit_middleware(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Extension(limiter): Extension<Arc<KeyedLimiter>>,
    req: axum::extract::Request,
    next: Next,
) -> axum::response::Response {
    let ip = addr.ip();
    match limiter.check_key(&ip) {
        Ok(_) => next.run(req).await,
        Err(_) => axum::response::Response::builder()
            .status(axum::http::StatusCode::TOO_MANY_REQUESTS)
            .header("retry-after", "1")
            .body(axum::body::Body::from("rate limit exceeded"))
            .unwrap(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_indexing(
    config: &Config,
    data_dir: &std::path::Path,
    max_file_size: usize,
    max_concurrent: usize,
    index_timeout_secs: u64,
    exclude_patterns: &[glob::Pattern],
    state: &Arc<AppState>,
    is_initial: bool,
    cancel: &tokio_util::sync::CancellationToken,
) {
    use crate::state::RepoStatus;
    use houndr_repo::pipeline::index_repo;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tracing::error;

    // Bridge async cancellation token to a sync AtomicBool for blocking tasks
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let flag_clone = cancel_flag.clone();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        cancel_clone.cancelled().await;
        flag_clone.store(true, Ordering::Relaxed);
    });

    // Clean up orphaned repos (removed from config but data still on disk)
    cleanup_orphaned_repos(config, data_dir, state).await;

    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent));

    // Spawn all indexing tasks concurrently (semaphore-gated)
    let mut handles = Vec::new();
    for repo_config in &config.repos {
        if cancel.is_cancelled() {
            break;
        }

        if is_initial {
            let mut statuses = state.repo_statuses.write().await;
            if !matches!(statuses.get(&repo_config.name), Some(RepoStatus::Ready)) {
                statuses.insert(repo_config.name.clone(), RepoStatus::Indexing);
            }
        }

        let sem = semaphore.clone();
        let repo_config = repo_config.clone();
        let data_dir = data_dir.to_path_buf();
        let exclude_patterns = exclude_patterns.to_vec();
        let repo_name = repo_config.name.clone();
        let flag = cancel_flag.clone();

        let handle = tokio::spawn(async move {
            // Safety: semaphore is Arc-cloned and never closed; lives for the task lifetime.
            let _permit = sem.acquire_owned().await.expect("semaphore closed");
            match tokio::time::timeout(
                Duration::from_secs(index_timeout_secs),
                tokio::task::spawn_blocking(move || {
                    let name = repo_config.name.clone();
                    let result = index_repo(
                        &repo_config,
                        &data_dir,
                        max_file_size,
                        &exclude_patterns,
                        &flag,
                    );
                    (name, result)
                }),
            )
            .await
            {
                Ok(Ok(result)) => result,
                Ok(Err(e)) => (repo_name, Err(anyhow::anyhow!("task panicked: {}", e))),
                Err(_) => (repo_name, Err(anyhow::anyhow!("indexing timed out"))),
            }
        });
        handles.push(handle);
    }

    // Collect results — merge-on-success: keep old readers for failed repos
    let old_readers = state.readers.read().await;
    let mut reader_map: std::collections::HashMap<String, Arc<houndr_index::IndexReader>> =
        old_readers
            .iter()
            .map(|r| (r.repo_name.clone(), r.clone()))
            .collect();
    drop(old_readers);

    for handle in handles {
        if cancel.is_cancelled() {
            handle.abort();
            continue;
        }
        match handle.await {
            Ok((name, Ok((reader, resolved_ref)))) => {
                info!(repo = %name, ref = %resolved_ref, "indexing complete");
                reader_map.insert(name.clone(), reader);
                let mut statuses = state.repo_statuses.write().await;
                statuses.insert(name.clone(), RepoStatus::Ready);
                drop(statuses);
                let mut refs = state.resolved_refs.write().await;
                refs.insert(name, resolved_ref);
            }
            Ok((name, Err(e))) => {
                if cancel.is_cancelled() {
                    continue;
                }
                error!(repo = %name, error = ?e, "indexing failed, keeping existing index");
                let mut statuses = state.repo_statuses.write().await;
                // Sanitize: log full error, expose only generic message via API
                let public_error = if format!("{}", e).contains("authentication")
                    || format!("{}", e).contains("auth")
                {
                    "authentication failed".to_string()
                } else if format!("{}", e).contains("not found") {
                    "repository or ref not found".to_string()
                } else {
                    "indexing failed".to_string()
                };
                statuses.insert(
                    name,
                    RepoStatus::Failed {
                        error: public_error,
                    },
                );
            }
            Err(e) => {
                if cancel.is_cancelled() {
                    continue;
                }
                error!(error = %e, "indexing task panicked");
            }
        }
    }

    if cancel.is_cancelled() {
        return;
    }

    let new_readers: Vec<_> = reader_map.into_values().collect();
    let count = new_readers.len();
    let mut readers = state.readers.write().await;
    *readers = new_readers;
    drop(readers);
    state.clear_cache().await;
    *state.last_watcher_heartbeat.write().await = Some(std::time::Instant::now());
    info!("index map updated with {} repos", count);
}

async fn cleanup_orphaned_repos(
    config: &Config,
    data_dir: &std::path::Path,
    state: &Arc<AppState>,
) {
    let configured: std::collections::HashSet<&str> =
        config.repos.iter().map(|r| r.name.as_str()).collect();

    let repos_dir = data_dir.join("repos");
    if let Ok(entries) = std::fs::read_dir(&repos_dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if !configured.contains(name) {
                    let path = entry.path();
                    if path.is_dir() {
                        if let Err(e) = std::fs::remove_dir_all(&path) {
                            tracing::warn!(repo = %name, error = %e, "failed to remove orphaned repo dir");
                        } else {
                            info!(repo = %name, "removed orphaned repo directory");
                        }
                    }

                    let idx_path = data_dir.join("indexes").join(format!("{}.idx", name));
                    if idx_path.exists() {
                        let _ = std::fs::remove_file(&idx_path);
                        info!(repo = %name, "removed orphaned index file");
                    }

                    let manifest_path = data_dir.join("manifests").join(format!("{}.json", name));
                    if manifest_path.exists() {
                        let _ = std::fs::remove_file(&manifest_path);
                        info!(repo = %name, "removed orphaned manifest file");
                    }

                    // Prune stale in-memory state
                    state.repo_statuses.write().await.remove(name);
                    state.resolved_refs.write().await.remove(name);
                }
            }
        }
    }

    // Also prune reader_map entries for repos no longer in config
    let mut readers = state.readers.write().await;
    let before = readers.len();
    readers.retain(|r| configured.contains(r.repo_name.as_str()));
    let removed = before - readers.len();
    if removed > 0 {
        info!(removed, "pruned stale readers for removed repos");
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install CTRL+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("shutdown signal received");

    // Force-quit on second signal — backstop for stuck blocking tasks
    tokio::spawn(async {
        tokio::signal::ctrl_c().await.ok();
        eprintln!("forced shutdown (second signal)");
        std::process::exit(1);
    });
}
