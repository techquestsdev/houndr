use lru::LruCache;
use serde::Serialize;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use houndr_index::IndexReader;
use houndr_repo::config::Config;

/// Per-repo indexing lifecycle status.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase", tag = "status")]
pub enum RepoStatus {
    Pending,
    Indexing,
    Ready,
    Failed { error: String },
}

/// Cached search result with TTL.
pub(crate) struct CachedResult {
    data: String, // JSON response
    created: Instant,
}

/// Application state shared across handlers.
pub struct AppState {
    /// Loaded configuration.
    pub config: Config,
    /// One memory-mapped index reader per successfully indexed repo.
    pub readers: RwLock<Vec<Arc<IndexReader>>>,
    /// LRU cache of serialized search results.
    pub cache: RwLock<LruCache<String, CachedResult>>,
    cache_ttl: Duration,
    /// Current indexing status for each configured repo.
    pub repo_statuses: RwLock<HashMap<String, RepoStatus>>,
    /// Resolved git refs per repo (auto-detected or from config).
    pub resolved_refs: RwLock<HashMap<String, String>>,
    /// Last time the watcher completed an indexing cycle.
    pub last_watcher_heartbeat: RwLock<Option<Instant>>,
    /// Configured poll interval (seconds) — used by healthz to detect stale watcher.
    pub poll_interval_secs: u64,
}

impl AppState {
    /// Create a new `AppState` with empty readers and pending statuses for all repos.
    pub fn new(config: Config) -> Self {
        let cache_size =
            NonZeroUsize::new(config.cache.max_entries).unwrap_or(NonZeroUsize::new(1000).unwrap());
        let cache_ttl = Duration::from_secs(config.cache.ttl_secs);
        let repo_statuses: HashMap<String, RepoStatus> = config
            .repos
            .iter()
            .map(|r| (r.name.clone(), RepoStatus::Pending))
            .collect();
        let poll_interval_secs = config.indexer.poll_interval_secs;
        Self {
            config,
            readers: RwLock::new(Vec::new()),
            cache: RwLock::new(LruCache::new(cache_size)),
            cache_ttl,
            repo_statuses: RwLock::new(repo_statuses),
            resolved_refs: RwLock::new(HashMap::new()),
            last_watcher_heartbeat: RwLock::new(None),
            poll_interval_secs,
        }
    }

    /// Get a cached search result if it exists and hasn't expired.
    pub async fn get_cached(&self, key: &str) -> Option<String> {
        let mut cache = self.cache.write().await;
        if let Some(entry) = cache.get(key) {
            if entry.created.elapsed() < self.cache_ttl {
                return Some(entry.data.clone());
            }
            // Expired — remove it
            cache.pop(key);
        }
        None
    }

    /// Clear all cached search results (e.g. after re-indexing).
    pub async fn clear_cache(&self) {
        self.cache.write().await.clear();
    }

    /// Store a search result in the cache.
    pub async fn set_cached(&self, key: String, data: String) {
        let mut cache = self.cache.write().await;
        cache.put(
            key,
            CachedResult {
                data,
                created: Instant::now(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use houndr_repo::config::{CacheConfig, Config, IndexerConfig, ServerConfig};

    fn test_config_with_cache(max_entries: usize, ttl_secs: u64) -> Config {
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
                max_entries,
                ttl_secs,
            },
            repos: vec![],
        }
    }

    #[tokio::test]
    async fn cache_set_get() {
        let state = AppState::new(test_config_with_cache(100, 300));
        state.set_cached("key1".into(), "value1".into()).await;
        assert_eq!(state.get_cached("key1").await, Some("value1".to_string()));
        assert_eq!(state.get_cached("nonexistent").await, None);
    }

    #[tokio::test]
    async fn cache_ttl_expiry() {
        // TTL of 0 seconds — entries expire immediately
        let config = test_config_with_cache(100, 0);
        let state = AppState::new(config);

        // Manually insert with an already-expired timestamp
        {
            let mut cache = state.cache.write().await;
            cache.put(
                "expired".into(),
                CachedResult {
                    data: "old".into(),
                    created: Instant::now() - Duration::from_secs(1),
                },
            );
        }
        assert_eq!(state.get_cached("expired").await, None);
    }

    #[tokio::test]
    async fn cache_lru_eviction() {
        // Cache with max 2 entries
        let state = AppState::new(test_config_with_cache(2, 300));
        state.set_cached("a".into(), "1".into()).await;
        state.set_cached("b".into(), "2".into()).await;
        state.set_cached("c".into(), "3".into()).await;

        // "a" should have been evicted (LRU)
        assert_eq!(state.get_cached("a").await, None);
        assert_eq!(state.get_cached("b").await, Some("2".into()));
        assert_eq!(state.get_cached("c").await, Some("3".into()));
    }
}
