use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{debug, info, warn};

use houndr_index::writer::write_index;
use houndr_index::{IndexBuilder, IndexReader};

use crate::config::RepoConfig;
use crate::vcs::GitRepo;

/// Index a single repository: clone/fetch, walk files, build and write index.
/// File contents are embedded directly in the index — no separate content store.
/// Uses incremental indexing when possible: only re-reads changed blobs from git.
/// If HEAD is unchanged and an index already exists, returns the existing reader.
/// Returns `(reader, resolved_ref)` where `resolved_ref` is the branch that was indexed.
pub fn index_repo(
    config: &RepoConfig,
    data_dir: &Path,
    max_file_size: usize,
    exclude_patterns: &[glob::Pattern],
    cancel: &Arc<AtomicBool>,
) -> Result<(Arc<IndexReader>, String)> {
    let (git_repo, fresh_clone) = GitRepo::clone_or_open(config, data_dir, cancel)?;
    let resolved_ref = git_repo.resolved_ref.clone();

    if cancel.load(Ordering::Relaxed) {
        anyhow::bail!("cancelled");
    }

    // Skip fetch for fresh clones — clone already has all objects
    if !fresh_clone {
        // Fetch latest — returns None if HEAD unchanged
        let new_sha = git_repo.fetch().context("fetch failed")?;

        if cancel.load(Ordering::Relaxed) {
            anyhow::bail!("cancelled");
        }

        // Skip reindex if HEAD unchanged and index already exists
        if new_sha.is_none() {
            if let Some(reader) = load_existing_index(&config.name, data_dir) {
                debug!(repo = %config.name, "HEAD unchanged, reusing existing index");
                return Ok((reader, resolved_ref));
            }
        }
    }

    // Try incremental indexing path
    let files =
        build_file_list_incremental(&git_repo, config, data_dir, max_file_size, exclude_patterns)?;

    if cancel.load(Ordering::Relaxed) {
        anyhow::bail!("cancelled");
    }

    // Build index (content is embedded)
    let mut builder = IndexBuilder::new();
    for (path, content) in &files {
        builder.add_doc(path.clone(), content.clone());
    }
    let built = builder.build();

    if cancel.load(Ordering::Relaxed) {
        anyhow::bail!("cancelled");
    }

    // Write index to disk
    let index_path = data_dir
        .join("indexes")
        .join(format!("{}.idx", config.name));
    if let Some(parent) = index_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_index(&built, &index_path)?;

    // Save tree manifest for next incremental cycle
    if let Ok(manifest) = git_repo.walk_tree_manifest(exclude_patterns) {
        let _ = save_manifest(data_dir, &config.name, &manifest);
    }

    info!(
        repo = %config.name,
        docs = files.len(),
        trigrams = built.postings.len(),
        "index written"
    );

    // Open the index for reading
    let reader = IndexReader::open(&index_path, config.name.clone())?;
    Ok((Arc::new(reader), resolved_ref))
}

/// Build the file list using incremental diffing when possible.
/// Reuses unchanged file content from the existing index (zero-copy mmap read).
/// Only reads changed/new blobs from git.
fn build_file_list_incremental(
    git_repo: &GitRepo,
    config: &RepoConfig,
    data_dir: &Path,
    max_file_size: usize,
    exclude_patterns: &[glob::Pattern],
) -> Result<Vec<(String, Vec<u8>)>> {
    let old_manifest = load_manifest(data_dir, &config.name);
    let old_reader = load_existing_index(&config.name, data_dir);

    // If we have both old manifest and old reader, do incremental
    if let (Some(old_manifest), Some(ref old_reader)) = (&old_manifest, &old_reader) {
        let new_manifest = git_repo.walk_tree_manifest(exclude_patterns)?;

        // Build path -> doc_id lookup from old reader
        let mut path_to_doc_id: HashMap<&str, u32> = HashMap::new();
        for doc_id in 0..old_reader.doc_count() {
            if let Some(path) = old_reader.doc_path(doc_id) {
                path_to_doc_id.insert(path, doc_id);
            }
        }

        let mut files = Vec::new();
        let mut reused = 0usize;
        let mut changed = 0usize;

        for (path, new_oid) in &new_manifest {
            if git_repo.is_cancelled() {
                anyhow::bail!("cancelled");
            }

            // Check if file is unchanged (same oid in old manifest)
            let is_unchanged = old_manifest
                .get(path)
                .map(|old_oid| old_oid == new_oid)
                .unwrap_or(false);

            if is_unchanged {
                // Try to reuse content from old index
                if let Some(&doc_id) = path_to_doc_id.get(path.as_str()) {
                    if let Some(content) = old_reader.doc_content(doc_id) {
                        files.push((path.clone(), content.to_vec()));
                        reused += 1;
                        continue;
                    }
                }
            }

            // Changed or new — read blob from git
            match git_repo.read_blob_checked(new_oid, max_file_size)? {
                Some(content) => {
                    files.push((path.clone(), content));
                    changed += 1;
                }
                None => {
                    // Filtered out (too large or binary)
                }
            }
        }

        info!(
            repo = %config.name,
            reused, changed,
            total = files.len(),
            "incremental index build"
        );
        return Ok(files);
    }

    // Fallback: full walk
    debug!(repo = %config.name, "no old manifest/index, doing full walk");
    git_repo.walk_files(max_file_size, exclude_patterns)
}

/// Try to load an existing index from disk (for startup).
pub fn load_existing_index(repo_name: &str, data_dir: &Path) -> Option<Arc<IndexReader>> {
    let index_path = data_dir.join("indexes").join(format!("{}.idx", repo_name));
    if !index_path.exists() {
        return None;
    }
    match IndexReader::open(&index_path, repo_name.to_string()) {
        Ok(reader) => {
            debug!(repo = %repo_name, "loaded existing index");
            Some(Arc::new(reader))
        }
        Err(e) => {
            warn!(repo = %repo_name, error = %e, "failed to load existing index");
            None
        }
    }
}

// --- Manifest persistence ---

fn manifest_path(data_dir: &Path, repo_name: &str) -> std::path::PathBuf {
    data_dir
        .join("manifests")
        .join(format!("{}.json", repo_name))
}

fn save_manifest(
    data_dir: &Path,
    repo_name: &str,
    manifest: &HashMap<String, String>,
) -> Result<()> {
    let path = manifest_path(data_dir, repo_name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string(manifest)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &path)?;
    debug!(repo = %repo_name, entries = manifest.len(), "manifest saved");
    Ok(())
}

fn load_manifest(data_dir: &Path, repo_name: &str) -> Option<HashMap<String, String>> {
    let path = manifest_path(data_dir, repo_name);
    let data = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}
