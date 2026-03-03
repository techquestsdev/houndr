use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};
use tracing::{error, info};

use crate::config::Config;
use crate::pipeline::index_repo;
use houndr_index::IndexReader;

/// Shared state: maps repo name → IndexReader.
pub type IndexMap = Arc<RwLock<Vec<Arc<IndexReader>>>>;

/// Start a polling loop that periodically re-indexes all configured repos.
pub async fn start_watcher(config: Config, index_map: IndexMap) {
    let data_dir = PathBuf::from(&config.indexer.data_dir);
    let poll_interval = Duration::from_secs(config.indexer.poll_interval_secs);
    let max_file_size = config.indexer.max_file_size;
    let max_concurrent = config.indexer.max_concurrent_indexers;
    let index_timeout_secs = config.indexer.index_timeout_secs;

    let exclude_patterns: Vec<glob::Pattern> = config
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

    // Initial indexing
    info!("starting initial indexing of {} repos", config.repos.len());
    run_indexing_cycle(
        &config,
        &data_dir,
        max_file_size,
        max_concurrent,
        index_timeout_secs,
        &exclude_patterns,
        &index_map,
    )
    .await;

    // Polling loop
    let mut ticker = interval(poll_interval);
    ticker.tick().await; // skip immediate tick

    loop {
        ticker.tick().await;
        info!("polling repos for changes");
        run_indexing_cycle(
            &config,
            &data_dir,
            max_file_size,
            max_concurrent,
            index_timeout_secs,
            &exclude_patterns,
            &index_map,
        )
        .await;
    }
}

async fn run_indexing_cycle(
    config: &Config,
    data_dir: &std::path::Path,
    max_file_size: usize,
    max_concurrent: usize,
    index_timeout_secs: u64,
    exclude_patterns: &[glob::Pattern],
    index_map: &IndexMap,
) {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent));

    let cancel_flag = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();

    for repo_config in &config.repos {
        let sem = semaphore.clone();
        let repo_config = repo_config.clone();
        let data_dir = data_dir.to_path_buf();
        let exclude_patterns = exclude_patterns.to_vec();
        let repo_name = repo_config.name.clone();
        let flag = cancel_flag.clone();

        let handle = tokio::spawn(async move {
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
                Ok(Err(e)) => (
                    repo_name.clone(),
                    Err(anyhow::anyhow!("task panicked: {}", e)),
                ),
                Err(_) => (
                    repo_name.clone(),
                    Err(anyhow::anyhow!("indexing timed out")),
                ),
            }
        });
        handles.push(handle);
    }

    // Merge-on-success: keep old readers for repos that fail
    let old_readers = index_map.read().await;
    let mut reader_map: std::collections::HashMap<String, Arc<IndexReader>> = old_readers
        .iter()
        .map(|r| (r.repo_name.clone(), r.clone()))
        .collect();
    drop(old_readers);

    for handle in handles {
        match handle.await {
            Ok((name, Ok((reader, resolved_ref)))) => {
                info!(repo = %name, ref = %resolved_ref, "indexing complete");
                reader_map.insert(name, reader);
            }
            Ok((name, Err(e))) => {
                error!(repo = %name, error = %e, "indexing failed, keeping existing index");
            }
            Err(e) => {
                error!(error = %e, "indexing task panicked");
            }
        }
    }

    let new_readers: Vec<_> = reader_map.into_values().collect();
    let count = new_readers.len();
    let mut map = index_map.write().await;
    *map = new_readers;
    info!("index map updated with {} repos", count);
}
