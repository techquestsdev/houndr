use anyhow::{Context, Result};
use git2::{FetchOptions, RemoteCallbacks, Repository};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{debug, info};

use crate::config::RepoConfig;

/// Manages a bare git repository clone.
pub struct GitRepo {
    /// Repository name from config.
    pub name: String,
    /// Path to the bare clone on disk.
    pub repo_path: PathBuf,
    /// Original repo configuration.
    pub config: RepoConfig,
    /// Resolved git ref (from config or auto-detected from remote HEAD).
    pub resolved_ref: String,
    /// Cancellation flag — checked by long-running operations.
    cancel: Arc<AtomicBool>,
}

impl GitRepo {
    /// Clone or open an existing bare repository.
    /// Returns `(Self, true)` if a fresh clone was performed, `(Self, false)` if opened existing.
    pub fn clone_or_open(
        config: &RepoConfig,
        data_dir: &Path,
        cancel: &Arc<AtomicBool>,
    ) -> Result<(Self, bool)> {
        let repo_path = data_dir.join("repos").join(&config.name);

        // Ensure parent directories exist for names containing '/'
        if let Some(parent) = repo_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let fresh_clone = if repo_path.exists() {
            debug!(repo = %config.name, "opening existing bare repo");
            // Verify it's a valid git repo
            Repository::open_bare(&repo_path)
                .with_context(|| format!("failed to open repo at {:?}", repo_path))?;
            false
        } else {
            info!(repo = %config.name, url = %config.url, "cloning bare repo");
            // Manual init + single-branch fetch instead of RepoBuilder::clone()
            // to avoid ref conflicts (e.g. branch "foo" vs "foo/bar" can't coexist).
            let repo = Repository::init_bare(&repo_path)
                .with_context(|| format!("failed to init bare repo at {:?}", repo_path))?;
            repo.remote("origin", &config.url)
                .with_context(|| format!("failed to add remote for {}", config.url))?;
            true
        };

        // Resolve the git ref: use explicit config, read cached detection, or query remote
        let ref_marker = repo_path.join("houndr_default_ref");
        let resolved_ref = if let Some(ref r) = config.git_ref {
            r.clone()
        } else if !fresh_clone {
            if let Ok(cached) = std::fs::read_to_string(&ref_marker) {
                let cached = cached.trim().to_string();
                if !cached.is_empty() {
                    debug!(repo = %config.name, ref = %cached, "using cached default ref");
                    cached
                } else {
                    "main".to_string()
                }
            } else {
                // Legacy repo without marker — try local HEAD, fall back to "main"
                let repo = Repository::open_bare(&repo_path)?;
                Self::detect_default_branch(&repo).unwrap_or_else(|| "main".to_string())
            }
        } else {
            let repo = Repository::open_bare(&repo_path)?;
            // No local HEAD yet — ask the remote for its default branch
            Self::detect_remote_default_branch(&repo, config).unwrap_or_else(|| "main".to_string())
        };

        // Persist detected ref so subsequent opens don't need to query the remote
        if fresh_clone || !ref_marker.exists() {
            let _ = std::fs::write(&ref_marker, &resolved_ref);
        }

        // For fresh clones, do the initial fetch of just the target branch
        if fresh_clone {
            let repo = Repository::open_bare(&repo_path)?;
            let mut callbacks = RemoteCallbacks::new();
            Self::setup_credentials(&mut callbacks, config);
            let cancel_flag = cancel.clone();
            callbacks.transfer_progress(move |_| !cancel_flag.load(Ordering::Relaxed));
            let mut fetch_opts = FetchOptions::new();
            fetch_opts.remote_callbacks(callbacks);
            let mut remote = repo.find_remote("origin")?;
            remote
                .fetch(&[&resolved_ref], Some(&mut fetch_opts), None)
                .with_context(|| format!("failed to fetch {} from {}", resolved_ref, config.url))?;
        }

        Ok((
            Self {
                name: config.name.clone(),
                repo_path,
                config: config.clone(),
                resolved_ref,
                cancel: cancel.clone(),
            },
            fresh_clone,
        ))
    }

    /// Check if cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Fetch latest changes. Returns the new HEAD SHA (or None if unchanged).
    pub fn fetch(&self) -> Result<Option<String>> {
        let repo = Repository::open_bare(&self.repo_path)?;

        let old_head = self.head_sha(&repo).ok();

        let mut callbacks = RemoteCallbacks::new();
        Self::setup_credentials(&mut callbacks, &self.config);
        let cancel = self.cancel.clone();
        callbacks.transfer_progress(move |progress| {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            debug!(
                received = progress.received_objects(),
                total = progress.total_objects(),
                "fetch progress"
            );
            true
        });

        let mut fetch_opts = FetchOptions::new();
        fetch_opts.remote_callbacks(callbacks);

        let mut remote = repo.find_remote("origin")?;
        remote.fetch(&[&self.resolved_ref], Some(&mut fetch_opts), None)?;

        let new_head = self.head_sha(&repo)?;

        if old_head.as_ref() == Some(&new_head) {
            debug!(repo = %self.name, sha = %new_head, "HEAD unchanged");
            Ok(None)
        } else {
            info!(repo = %self.name, sha = %new_head, "HEAD updated");
            Ok(Some(new_head))
        }
    }

    /// Detect the default branch from the bare repo's HEAD reference.
    fn detect_default_branch(repo: &Repository) -> Option<String> {
        let head = repo.head().ok()?;
        head.shorthand().map(|s| s.to_string())
    }

    /// Detect the default branch by querying the remote (for fresh clones with no local HEAD).
    fn detect_remote_default_branch(repo: &Repository, config: &RepoConfig) -> Option<String> {
        let mut callbacks = RemoteCallbacks::new();
        Self::setup_credentials(&mut callbacks, config);
        let mut remote = repo.find_remote("origin").ok()?;
        remote
            .connect_auth(git2::Direction::Fetch, Some(callbacks), None)
            .ok()?;
        let default_branch = remote.default_branch().ok()?;
        let name = default_branch.as_str()?;
        // Strip "refs/heads/" prefix
        let short = name.strip_prefix("refs/heads/").unwrap_or(name);
        let result = short.to_string();
        remote.disconnect().ok();
        Some(result)
    }

    /// Set up git2 credentials callback based on repo config.
    ///
    /// libgit2 calls the callback repeatedly on auth failure. We track attempts
    /// so explicit-key methods fall back to the SSH agent on retry, rather than
    /// looping on the same failing credential.
    fn setup_credentials(callbacks: &mut RemoteCallbacks, config: &RepoConfig) {
        use std::sync::atomic::{AtomicU32, Ordering};

        if let Some(token) = &config.auth_token {
            let token = token.clone();
            callbacks.credentials(move |_url, username, _allowed| {
                git2::Cred::userpass_plaintext(username.unwrap_or("git"), &token)
            });
        } else if let Some(key_content) = &config.ssh_key {
            // In-memory SSH key, fall back to SSH agent on retry
            let key_content = key_content.clone();
            let passphrase = config.ssh_key_passphrase.clone();
            let attempt = AtomicU32::new(0);
            callbacks.credentials(move |_url, username, allowed| {
                let n = attempt.fetch_add(1, Ordering::Relaxed);
                let user = username.unwrap_or("git");
                if n == 0 {
                    git2::Cred::ssh_key_from_memory(user, None, &key_content, passphrase.as_deref())
                } else if allowed.contains(git2::CredentialType::SSH_KEY) {
                    git2::Cred::ssh_key_from_agent(user)
                } else {
                    Err(git2::Error::from_str(
                        "all authentication methods exhausted",
                    ))
                }
            });
        } else if let Some(key_path) = &config.ssh_key_path {
            // SSH key from file path, fall back to SSH agent on retry
            let key_path = key_path.clone();
            let passphrase = config.ssh_key_passphrase.clone();
            let attempt = AtomicU32::new(0);
            callbacks.credentials(move |_url, username, allowed| {
                let n = attempt.fetch_add(1, Ordering::Relaxed);
                let user = username.unwrap_or("git");
                if n == 0 {
                    git2::Cred::ssh_key(user, None, Path::new(&key_path), passphrase.as_deref())
                } else if allowed.contains(git2::CredentialType::SSH_KEY) {
                    git2::Cred::ssh_key_from_agent(user)
                } else {
                    Err(git2::Error::from_str(
                        "all authentication methods exhausted",
                    ))
                }
            });
        } else {
            // No explicit auth — try the SSH agent
            callbacks.credentials(move |_url, username, allowed| {
                let user = username.unwrap_or("git");
                if allowed.contains(git2::CredentialType::SSH_KEY) {
                    git2::Cred::ssh_key_from_agent(user)
                } else {
                    Err(git2::Error::from_str("no credentials configured"))
                }
            });
        }
    }

    /// Get the HEAD commit SHA for the configured ref.
    fn head_sha(&self, repo: &Repository) -> Result<String> {
        let reference = repo
            .find_reference(&format!("refs/heads/{}", self.resolved_ref))
            .or_else(|_| repo.find_reference(&format!("refs/remotes/origin/{}", self.resolved_ref)))
            .with_context(|| format!("ref {} not found", self.resolved_ref))?;
        let oid = reference
            .peel_to_commit()
            .context("failed to peel to commit")?
            .id();
        Ok(oid.to_string())
    }

    /// Walk the tree at HEAD and yield (path, content) pairs.
    /// Skips binary files and files larger than max_file_size.
    pub fn walk_files(
        &self,
        max_file_size: usize,
        exclude_patterns: &[glob::Pattern],
    ) -> Result<Vec<(String, Vec<u8>)>> {
        let repo = Repository::open_bare(&self.repo_path)?;

        let reference = repo
            .find_reference(&format!("refs/heads/{}", self.resolved_ref))
            .or_else(|_| {
                repo.find_reference(&format!("refs/remotes/origin/{}", self.resolved_ref))
            })?;
        let commit = reference.peel_to_commit()?;
        let tree = commit.tree()?;

        let mut files = Vec::new();

        let cancel = self.cancel.clone();
        tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
            if cancel.load(Ordering::Relaxed) {
                return git2::TreeWalkResult::Abort;
            }

            if entry.kind() != Some(git2::ObjectType::Blob) {
                return git2::TreeWalkResult::Ok;
            }

            // Skip symlinks (git filemode 120000)
            if entry.filemode() == 0o120000 {
                return git2::TreeWalkResult::Ok;
            }

            let name = match entry.name() {
                Some(n) => n,
                None => return git2::TreeWalkResult::Ok,
            };

            let path = if dir.is_empty() {
                name.to_string()
            } else {
                format!("{}{}", dir, name)
            };

            // Check exclude patterns
            for pattern in exclude_patterns {
                if pattern.matches(&path) {
                    return git2::TreeWalkResult::Ok;
                }
            }

            let blob = match entry.to_object(&repo).and_then(|o| o.peel_to_blob()) {
                Ok(b) => b,
                Err(_) => return git2::TreeWalkResult::Ok,
            };

            // Check raw byte length before reading content
            if blob.size() > max_file_size {
                return git2::TreeWalkResult::Ok;
            }

            let content = blob.content();

            // Skip binary files (null byte in first 8KB)
            let check_len = content.len().min(8192);
            if content[..check_len].contains(&0) {
                return git2::TreeWalkResult::Ok;
            }

            files.push((path, content.to_vec()));
            git2::TreeWalkResult::Ok
        })?;

        info!(repo = %self.name, files = files.len(), "walked tree");
        Ok(files)
    }

    /// Walk the tree at HEAD and return a manifest: `path -> blob OID hex`.
    /// Does NOT read blob content (cheap tree traversal).
    pub fn walk_tree_manifest(
        &self,
        exclude_patterns: &[glob::Pattern],
    ) -> Result<HashMap<String, String>> {
        let repo = Repository::open_bare(&self.repo_path)?;

        let reference = repo
            .find_reference(&format!("refs/heads/{}", self.resolved_ref))
            .or_else(|_| {
                repo.find_reference(&format!("refs/remotes/origin/{}", self.resolved_ref))
            })?;
        let commit = reference.peel_to_commit()?;
        let tree = commit.tree()?;

        let mut manifest = HashMap::new();

        let cancel = self.cancel.clone();
        tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
            if cancel.load(Ordering::Relaxed) {
                return git2::TreeWalkResult::Abort;
            }

            if entry.kind() != Some(git2::ObjectType::Blob) {
                return git2::TreeWalkResult::Ok;
            }

            // Skip symlinks (git filemode 120000)
            if entry.filemode() == 0o120000 {
                return git2::TreeWalkResult::Ok;
            }

            let name = match entry.name() {
                Some(n) => n,
                None => return git2::TreeWalkResult::Ok,
            };

            let path = if dir.is_empty() {
                name.to_string()
            } else {
                format!("{}{}", dir, name)
            };

            for pattern in exclude_patterns {
                if pattern.matches(&path) {
                    return git2::TreeWalkResult::Ok;
                }
            }

            manifest.insert(path, entry.id().to_string());
            git2::TreeWalkResult::Ok
        })?;

        debug!(repo = %self.name, entries = manifest.len(), "walked tree manifest");
        Ok(manifest)
    }

    /// Read a single blob by OID hex, applying size and binary filters.
    /// Returns `None` if the blob is too large or binary.
    pub fn read_blob_checked(
        &self,
        oid_hex: &str,
        max_file_size: usize,
    ) -> Result<Option<Vec<u8>>> {
        let repo = Repository::open_bare(&self.repo_path)?;
        let oid =
            git2::Oid::from_str(oid_hex).with_context(|| format!("invalid oid: {}", oid_hex))?;

        // Pre-check blob size via ODB header to avoid loading multi-GB blobs
        let odb = repo.odb()?;
        let (size, _obj_type) = odb.read_header(oid)?;
        if size > max_file_size {
            return Ok(None);
        }

        let blob = repo.find_blob(oid)?;
        let content = blob.content();

        let check_len = content.len().min(8192);
        if content[..check_len].contains(&0) {
            return Ok(None);
        }

        Ok(Some(content.to_vec()))
    }
}
