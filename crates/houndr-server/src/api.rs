use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Json, Response};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_stream::wrappers::ReceiverStream;

use crate::state::{AppState, RepoStatus};
use houndr_index::query::{execute_search, QueryPlan, SearchResult};

/// Query parameters for search endpoints.
#[derive(Debug, Deserialize)]
pub struct SearchParams {
    /// Search query string.
    pub q: String,
    /// Comma-separated repo names to search.
    #[serde(default)]
    pub repos: Option<String>,
    /// Glob pattern to filter file paths.
    #[serde(default)]
    pub files: Option<String>,
    /// Case-insensitive matching.
    #[serde(default)]
    pub i: Option<bool>,
    /// Treat query as regex.
    #[serde(default)]
    pub regex: Option<bool>,
    /// Max file matches to return per repo. 0 = use server default.
    #[serde(default)]
    pub max: usize,
}
const MAX_QUERY_LENGTH: usize = 4096;

/// JSON response body for search results.
#[derive(Debug, Serialize)]
pub struct SearchResponse {
    /// Per-repo search results.
    pub results: Vec<SearchResult>,
    /// Total search time in milliseconds.
    pub duration_ms: f64,
    /// Total files with matches across all repos.
    pub total_files: usize,
    /// Total matching lines across all files and repos.
    pub total_matches: usize,
    /// Whether results were truncated due to the max results limit.
    pub truncated: bool,
}

fn parse_file_pattern(files: Option<&String>) -> Option<glob::Pattern> {
    let f = files?;
    if f.contains("..") {
        tracing::warn!(pattern = %f, "rejected file pattern containing '..'");
        return None;
    }
    match glob::Pattern::new(f) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(pattern = %f, error = %e, "invalid file glob pattern");
            None
        }
    }
}

/// JSON error response body.
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    /// Human-readable error message.
    pub error: String,
}

/// Summary info for an indexed repository.
#[derive(Debug, Serialize)]
pub struct RepoInfo {
    /// Repository name.
    pub name: String,
    /// Number of indexed documents.
    pub doc_count: u32,
    /// Number of unique trigrams.
    pub trigram_count: u32,
}

#[derive(Debug, Serialize)]
struct StreamDone {
    duration_ms: f64,
    total_files: usize,
}

/// JSON search handler — returns all results in a single response.
pub async fn search_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchParams>,
) -> Response {
    if params.q.len() > MAX_QUERY_LENGTH {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!(
                    "query too long ({} bytes, max {})",
                    params.q.len(),
                    MAX_QUERY_LENGTH
                ),
            }),
        )
            .into_response();
    }

    let case_insensitive = params.i.unwrap_or(false);
    let is_regex = params.regex.unwrap_or(false);

    // Build cache key
    let cache_key = format!(
        "q={}&repos={}&files={}&i={}&regex={}&max={}",
        params.q,
        params.repos.as_deref().unwrap_or(""),
        params.files.as_deref().unwrap_or(""),
        case_insensitive,
        is_regex,
        params.max
    );

    // Check cache — return raw JSON string directly
    if let Some(cached) = state.get_cached(&cache_key).await {
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            cached,
        )
            .into_response();
    }

    // Build query plan
    let plan = match QueryPlan::new(&params.q, is_regex, case_insensitive) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(query = %params.q, error = %e, "search query rejected");
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
                .into_response();
        }
    };

    let file_pattern = parse_file_pattern(params.files.as_ref());

    let repo_filter: Option<Vec<String>> = params
        .repos
        .as_ref()
        .map(|r| r.split(',').map(|s| s.trim().to_string()).collect());

    let start = Instant::now();

    let readers = state.readers.read().await;
    let resolved_refs = state.resolved_refs.read().await;
    let server_max = state.config.server.max_search_results;
    let max_results = if params.max == 0 {
        server_max
    } else {
        params.max.min(server_max)
    };

    // Filter readers by repo name
    let filtered_readers: Vec<_> = readers
        .iter()
        .filter(|reader| {
            if let Some(ref filter) = repo_filter {
                filter.iter().any(|f| f == &reader.repo_name)
            } else {
                true
            }
        })
        .collect();

    // Search across all repos in parallel
    let results: Vec<SearchResult> = filtered_readers
        .par_iter()
        .filter_map(|reader| {
            let mut result = execute_search(
                reader,
                &plan,
                max_results,
                file_pattern.as_ref(),
                case_insensitive,
            );

            if result.files.is_empty() {
                return None;
            }

            // Attach repo URL and ref from config
            if let Some(repo_cfg) = state.config.repos.iter().find(|r| r.name == result.repo) {
                result.url = Some(repo_cfg.url.clone());
                result.git_ref = repo_cfg
                    .git_ref
                    .clone()
                    .or_else(|| resolved_refs.get(&result.repo).cloned());
            }
            Some(result)
        })
        .collect();
    drop(resolved_refs);

    let duration_ms = start.elapsed().as_secs_f64() * 1000.0;
    let total_files: usize = results.iter().map(|r| r.total_file_count).sum();
    let total_matches: usize = results.iter().map(|r| r.total_match_count).sum();
    let truncated = results.iter().any(|r| r.total_file_count > r.files.len());

    let response = SearchResponse {
        results,
        duration_ms,
        total_files,
        total_matches,
        truncated,
    };

    // Cache the result (skip empty results to avoid caching during indexing)
    if total_files > 0 {
        if let Ok(json) = serde_json::to_string(&response) {
            state.set_cached(cache_key, json).await;
        }
    }

    Json(response).into_response()
}

/// SSE streaming search endpoint — sends results per-repo as they complete.
/// Events: "result" (one per repo with matches), "done" (final summary).
pub async fn search_stream_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchParams>,
) -> Response {
    if params.q.len() > MAX_QUERY_LENGTH {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!(
                    "query too long ({} bytes, max {})",
                    params.q.len(),
                    MAX_QUERY_LENGTH
                ),
            }),
        )
            .into_response();
    }

    let case_insensitive = params.i.unwrap_or(false);
    let is_regex = params.regex.unwrap_or(false);

    let plan = match QueryPlan::new(&params.q, is_regex, case_insensitive) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            tracing::warn!(query = %params.q, error = %e, "search query rejected");
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
                .into_response();
        }
    };

    let file_pattern = parse_file_pattern(params.files.as_ref());

    let repo_filter: Option<Vec<String>> = params
        .repos
        .as_ref()
        .map(|r| r.split(',').map(|s| s.trim().to_string()).collect());

    let readers = state.readers.read().await.clone();
    let server_max = state.config.server.max_search_results;
    let max_results = if params.max == 0 {
        server_max
    } else {
        params.max.min(server_max)
    };
    let config_repos = state.config.repos.clone();
    let resolved_refs_map = state.resolved_refs.read().await.clone();

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(32);

    tokio::spawn(async move {
        let start = Instant::now();
        let mut total_files = 0usize;
        let mut handles = Vec::new();
        let tx_done = tx.clone(); // Keep one sender for the "done" event

        for reader in readers.iter() {
            if let Some(ref filter) = repo_filter {
                if !filter.iter().any(|f| f == &reader.repo_name) {
                    continue;
                }
            }

            let reader = reader.clone();
            let plan = plan.clone();
            let file_pattern = file_pattern.clone();
            let tx = tx.clone();
            let config_repos = config_repos.clone();
            let resolved_refs_map = resolved_refs_map.clone();

            let handle = tokio::task::spawn_blocking(move || {
                let mut result = execute_search(
                    &reader,
                    &plan,
                    max_results,
                    file_pattern.as_ref(),
                    case_insensitive,
                );

                if result.total_file_count == 0 {
                    return 0;
                }

                if let Some(repo_cfg) = config_repos.iter().find(|r| r.name == result.repo) {
                    result.url = Some(repo_cfg.url.clone());
                    result.git_ref = repo_cfg
                        .git_ref
                        .clone()
                        .or_else(|| resolved_refs_map.get(&result.repo).cloned());
                }

                let count = result.total_file_count;
                if let Ok(json) = serde_json::to_string(&result) {
                    let event = Event::default().event("result").data(json);
                    let _ = tx.blocking_send(Ok(event));
                }
                count
            });
            handles.push(handle);
        }

        drop(tx); // Drop loop sender — only tx_done remains

        for handle in handles {
            if let Ok(count) = handle.await {
                total_files += count;
            }
        }

        // Send final "done" event with summary
        let done = StreamDone {
            duration_ms: start.elapsed().as_secs_f64() * 1000.0,
            total_files,
        };
        if let Ok(json) = serde_json::to_string(&done) {
            let event = Event::default().event("done").data(json);
            let _ = tx_done.send(Ok(event)).await;
        }
        // tx_done drops here, closing the stream
    });

    Sse::new(ReceiverStream::new(rx)).into_response()
}

/// List all indexed repositories with document and trigram counts.
pub async fn repos_handler(State(state): State<Arc<AppState>>) -> Json<Vec<RepoInfo>> {
    let readers = state.readers.read().await;
    let repos: Vec<RepoInfo> = readers
        .iter()
        .map(|r| RepoInfo {
            name: r.repo_name.clone(),
            doc_count: r.doc_count(),
            trigram_count: r.trigram_count(),
        })
        .collect();
    Json(repos)
}

/// Health check response body.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    /// `"ready"` or `"initializing"`.
    pub status: &'static str,
    /// Number of repos with a loaded index.
    pub repos_indexed: usize,
    /// Total documents across all indexed repos.
    pub total_docs: u64,
    /// Watcher liveness: `"ok"`, `"stale"`, or absent if no heartbeat yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watcher_status: Option<&'static str>,
}

/// Per-repo indexing status (pending, indexing, ready, failed).
pub async fn status_handler(
    State(state): State<Arc<AppState>>,
) -> Json<std::collections::HashMap<String, RepoStatus>> {
    Json(state.repo_statuses.read().await.clone())
}

/// Health check — returns 200 when repos are indexed, 503 otherwise.
/// Also checks watcher liveness via heartbeat.
pub async fn healthz_handler(State(state): State<Arc<AppState>>) -> Response {
    let readers = state.readers.read().await;
    let repos_indexed = readers.len();
    let total_docs: u64 = readers.iter().map(|r| r.doc_count() as u64).sum();
    let is_ready = repos_indexed > 0;

    let watcher_status = {
        let hb = state.last_watcher_heartbeat.read().await;
        hb.map(|ts| {
            let max_elapsed = Duration::from_secs(state.poll_interval_secs * 2 + 30);
            if ts.elapsed() > max_elapsed {
                "stale"
            } else {
                "ok"
            }
        })
    };

    let body = HealthResponse {
        status: if is_ready { "ready" } else { "initializing" },
        repos_indexed,
        total_docs,
        watcher_status,
    };
    let status = if is_ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use axum::Router;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use houndr_index::writer::write_index;
    use houndr_index::{IndexBuilder, IndexReader};
    use houndr_repo::config::{CacheConfig, Config, IndexerConfig, RepoConfig, ServerConfig};

    fn test_config(repos: Vec<RepoConfig>) -> Config {
        Config {
            server: ServerConfig {
                bind: "127.0.0.1:0".into(),
                timeout_secs: 30,
                cors_origins: vec![],
                rate_limit_rps: 0,
                max_request_bytes: 1_048_576,
                max_search_results: 10_000,
            },
            indexer: IndexerConfig {
                data_dir: "/tmp/houndr-test".into(),
                max_concurrent_indexers: 1,
                poll_interval_secs: 60,
                max_file_size: 1_048_576,
                exclude_patterns: vec![],
                index_timeout_secs: 300,
            },
            cache: CacheConfig {
                max_entries: 1000,
                ttl_secs: 300,
            },
            repos,
        }
    }

    fn test_app(state: Arc<AppState>) -> Router {
        Router::new()
            .route("/api/v1/search", get(search_handler))
            .route("/api/v1/repos", get(repos_handler))
            .route("/api/v1/status", get(status_handler))
            .route("/healthz", get(healthz_handler))
            .with_state(state)
    }

    async fn get_body(response: axum::response::Response) -> String {
        let body = response.into_body();
        let bytes = body.collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn build_test_reader(dir: &std::path::Path, repo_name: &str) -> Arc<IndexReader> {
        let path = dir.join(format!("{}.idx", repo_name));
        let mut builder = IndexBuilder::new();
        builder.add_doc(
            "src/main.rs".into(),
            b"fn main() {\n    println!(\"hello world\");\n}".to_vec(),
        );
        builder.add_doc(
            "src/lib.rs".into(),
            b"pub fn greet() {\n    println!(\"hi\");\n}".to_vec(),
        );
        let built = builder.build();
        write_index(&built, &path).unwrap();
        Arc::new(IndexReader::open(&path, repo_name.into()).unwrap())
    }

    #[tokio::test]
    async fn healthz_initializing() {
        let state = Arc::new(AppState::new(test_config(vec![])));
        let app = test_app(state);

        let response = app
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = get_body(response).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["status"], "initializing");
    }

    #[tokio::test]
    async fn healthz_ready() {
        let state = Arc::new(AppState::new(test_config(vec![])));
        let dir = tempfile::tempdir().unwrap();
        let reader = build_test_reader(dir.path(), "test-repo");
        {
            let mut readers = state.readers.write().await;
            readers.push(reader);
        }

        let app = test_app(state);
        let response = app
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = get_body(response).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["status"], "ready");
        assert_eq!(json["repos_indexed"], 1);
    }

    #[tokio::test]
    async fn repos_empty() {
        let state = Arc::new(AppState::new(test_config(vec![])));
        let app = test_app(state);

        let response = app
            .oneshot(Request::get("/api/v1/repos").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = get_body(response).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json, serde_json::json!([]));
    }

    #[tokio::test]
    async fn repos_with_data() {
        let state = Arc::new(AppState::new(test_config(vec![])));
        let dir = tempfile::tempdir().unwrap();
        let reader = build_test_reader(dir.path(), "my-repo");
        let expected_docs = reader.doc_count();
        let expected_trigrams = reader.trigram_count();
        {
            let mut readers = state.readers.write().await;
            readers.push(reader);
        }

        let app = test_app(state);
        let response = app
            .oneshot(Request::get("/api/v1/repos").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = get_body(response).await;
        let json: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(json.len(), 1);
        assert_eq!(json[0]["name"], "my-repo");
        assert_eq!(json[0]["doc_count"], expected_docs);
        assert_eq!(json[0]["trigram_count"], expected_trigrams);
    }

    #[tokio::test]
    async fn status_pending() {
        let repos = vec![RepoConfig {
            name: "test-repo".into(),
            url: "https://example.com/test.git".into(),
            git_ref: Some("main".into()),
            auth_token: None,
            ssh_key: None,
            ssh_key_path: None,
            ssh_key_passphrase: None,
        }];
        let state = Arc::new(AppState::new(test_config(repos)));
        let app = test_app(state);

        let response = app
            .oneshot(Request::get("/api/v1/status").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = get_body(response).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["test-repo"]["status"], "pending");
    }

    #[tokio::test]
    async fn search_empty_query() {
        let state = Arc::new(AppState::new(test_config(vec![])));
        let app = test_app(state);

        let response = app
            .oneshot(
                Request::get("/api/v1/search?q=ab")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn search_query_too_long() {
        let state = Arc::new(AppState::new(test_config(vec![])));
        let app = test_app(state);

        let long_query: String = "a".repeat(4097);
        let uri = format!("/api/v1/search?q={}", long_query);
        let response = app
            .oneshot(Request::get(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = get_body(response).await;
        assert!(body.contains("too long"));
    }

    #[tokio::test]
    async fn search_invalid_regex() {
        let state = Arc::new(AppState::new(test_config(vec![])));
        let app = test_app(state);

        let response = app
            .oneshot(
                Request::get("/api/v1/search?q=(unclosed&regex=true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn search_no_results() {
        let state = Arc::new(AppState::new(test_config(vec![])));
        let dir = tempfile::tempdir().unwrap();
        let reader = build_test_reader(dir.path(), "my-repo");
        {
            let mut readers = state.readers.write().await;
            readers.push(reader);
        }

        let app = test_app(state);
        let response = app
            .oneshot(
                Request::get("/api/v1/search?q=zzzzzznotfound")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = get_body(response).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["total_files"], 0);
        assert!(json["results"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn search_with_results() {
        let repos = vec![RepoConfig {
            name: "my-repo".into(),
            url: "https://example.com/test.git".into(),
            git_ref: Some("main".into()),
            auth_token: None,
            ssh_key: None,
            ssh_key_path: None,
            ssh_key_passphrase: None,
        }];
        let state = Arc::new(AppState::new(test_config(repos)));
        let dir = tempfile::tempdir().unwrap();
        let reader = build_test_reader(dir.path(), "my-repo");
        {
            let mut readers = state.readers.write().await;
            readers.push(reader);
        }

        let app = test_app(state);
        let response = app
            .oneshot(
                Request::get("/api/v1/search?q=println")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = get_body(response).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(json["total_files"].as_u64().unwrap() > 0);
        let results = json["results"].as_array().unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0]["repo"], "my-repo");
    }

    #[tokio::test]
    async fn search_repo_filter() {
        let state = Arc::new(AppState::new(test_config(vec![])));
        let dir = tempfile::tempdir().unwrap();

        let reader_a = build_test_reader(dir.path(), "repo-a");
        // Build a second index with different content
        let path_b = dir.path().join("repo-b.idx");
        let mut builder = IndexBuilder::new();
        builder.add_doc("unique.rs".into(), b"fn unique_function_xyz() {}".to_vec());
        let built = builder.build();
        write_index(&built, &path_b).unwrap();
        let reader_b = Arc::new(IndexReader::open(&path_b, "repo-b".into()).unwrap());

        {
            let mut readers = state.readers.write().await;
            readers.push(reader_a);
            readers.push(reader_b);
        }

        let app = test_app(state);
        // Search only repo-b
        let response = app
            .oneshot(
                Request::get("/api/v1/search?q=unique_function_xyz&repos=repo-b")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = get_body(response).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let results = json["results"].as_array().unwrap();
        // Should only have results from repo-b
        for result in results {
            assert_eq!(result["repo"], "repo-b");
        }
    }

    #[tokio::test]
    async fn search_max_capped() {
        let state = Arc::new(AppState::new(test_config(vec![])));
        let app = test_app(state);

        // max=99999 should not cause an error — it's silently capped to server max (10000)
        let response = app
            .oneshot(
                Request::get("/api/v1/search?q=test&max=99999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Should succeed (200 OK with empty results since no repos)
        assert_eq!(response.status(), StatusCode::OK);
    }
}
