use serde::Deserialize;

/// Top-level application configuration loaded from `config.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// HTTP server settings.
    pub server: ServerConfig,
    /// Indexing pipeline settings.
    pub indexer: IndexerConfig,
    /// Search result cache settings.
    #[serde(default)]
    pub cache: CacheConfig,
    /// Repositories to index.
    #[serde(default)]
    pub repos: Vec<RepoConfig>,
}

/// HTTP server configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Address and port to listen on.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Request timeout in seconds.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// CORS allowed origins. Empty = permissive (allow all).
    #[serde(default)]
    pub cors_origins: Vec<String>,
    /// Max requests per second per IP address. 0 = unlimited.
    #[serde(default = "default_rate_limit_rps")]
    pub rate_limit_rps: u64,
    /// Max request body size in bytes. Default: 1MB.
    #[serde(default = "default_max_request_bytes")]
    pub max_request_bytes: usize,
    /// Max search results per repo per query. Default: 10000.
    #[serde(default = "default_max_search_results")]
    pub max_search_results: usize,
}

/// Indexing pipeline configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexerConfig {
    /// Directory for cloned repos, indexes, and manifests.
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    /// Max repos indexed in parallel.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_indexers: usize,
    /// Seconds between re-index polls.
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// Skip files larger than this (bytes).
    #[serde(default = "default_max_file_size")]
    pub max_file_size: usize,
    /// Glob patterns to exclude from indexing.
    #[serde(default)]
    pub exclude_patterns: Vec<String>,
    /// Per-repo indexing timeout in seconds.
    #[serde(default = "default_index_timeout_secs")]
    pub index_timeout_secs: u64,
}

/// Search result cache configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    /// Max cached search results (LRU eviction).
    #[serde(default = "default_cache_entries")]
    pub max_entries: usize,
    /// Cache entry TTL in seconds.
    #[serde(default = "default_cache_ttl")]
    pub ttl_secs: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_entries: default_cache_entries(),
            ttl_secs: default_cache_ttl(),
        }
    }
}

/// Configuration for a single Git repository.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    /// Unique identifier for this repo.
    pub name: String,
    /// Git clone URL (HTTPS or SSH).
    pub url: String,
    /// Branch or tag to index. If omitted, uses the remote's default branch.
    #[serde(default)]
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    /// HTTPS token for private repos (e.g. GitLab PAT, GitHub PAT).
    #[serde(default)]
    pub auth_token: Option<String>,
    /// SSH private key content (PEM string). Use `$ENV_VAR` to read from env.
    /// Takes priority over `ssh_key_path`.
    #[serde(default)]
    pub ssh_key: Option<String>,
    /// Path to SSH private key file on disk.
    #[serde(default)]
    pub ssh_key_path: Option<String>,
    /// Passphrase for SSH private key (if encrypted).
    #[serde(default)]
    pub ssh_key_passphrase: Option<String>,
}

fn default_bind() -> String {
    "127.0.0.1:6080".into()
}
fn default_timeout_secs() -> u64 {
    30
}
fn default_data_dir() -> String {
    "data".into()
}
fn default_max_concurrent() -> usize {
    4
}
fn default_poll_interval() -> u64 {
    60
}
fn default_max_file_size() -> usize {
    1_048_576
}
fn default_index_timeout_secs() -> u64 {
    300
}
fn default_cache_entries() -> usize {
    1000
}
fn default_cache_ttl() -> u64 {
    300
}
fn default_rate_limit_rps() -> u64 {
    0
}
fn default_max_request_bytes() -> usize {
    1_048_576
}
fn default_max_search_results() -> usize {
    10_000
}
impl Config {
    /// Load configuration from a TOML file, validate, and resolve env vars.
    pub fn from_file(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut config: Config = toml::from_str(&content)?;
        config.validate()?;
        config.resolve_env_vars();
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.repos.is_empty() {
            tracing::warn!("no repos configured — server will start but have nothing to index");
        }
        if self.server.timeout_secs == 0 {
            anyhow::bail!("server.timeout_secs must be > 0");
        }
        if self.cache.max_entries == 0 {
            anyhow::bail!("cache.max_entries must be > 0");
        }
        if self.indexer.max_concurrent_indexers == 0 {
            anyhow::bail!("indexer.max_concurrent_indexers must be > 0");
        }
        if self.indexer.poll_interval_secs == 0 {
            anyhow::bail!("indexer.poll_interval_secs must be > 0");
        }
        if self.indexer.max_file_size == 0 {
            anyhow::bail!("indexer.max_file_size must be > 0");
        }
        if self.server.max_request_bytes == 0 {
            anyhow::bail!("server.max_request_bytes must be > 0");
        }
        for origin in &self.server.cors_origins {
            if origin.is_empty() || !origin.starts_with("http") {
                anyhow::bail!(
                    "invalid CORS origin (must start with http:// or https://): {}",
                    origin
                );
            }
        }
        let mut seen = std::collections::HashSet::new();
        for repo in &self.repos {
            if repo.name.is_empty() {
                anyhow::bail!("repo name cannot be empty");
            }
            if repo.url.is_empty() {
                anyhow::bail!("repo url cannot be empty for repo '{}'", repo.name);
            }
            validate_repo_url(&repo.url, &repo.name)?;
            if let Some(ref git_ref) = repo.git_ref {
                validate_git_ref(git_ref, &repo.name)?;
            }
            if !seen.insert(&repo.name) {
                anyhow::bail!("duplicate repo name: {}", repo.name);
            }
        }
        Ok(())
    }

    /// Resolve `$VAR` / `${VAR}` references in auth fields from environment variables.
    fn resolve_env_vars(&mut self) {
        for repo in &mut self.repos {
            repo.auth_token = repo.auth_token.take().and_then(|v| resolve_env_opt(&v));
            repo.ssh_key = repo.ssh_key.take().and_then(|v| resolve_env_opt(&v));
            repo.ssh_key_path = repo.ssh_key_path.take().and_then(|v| resolve_env_opt(&v));
            repo.ssh_key_passphrase = repo
                .ssh_key_passphrase
                .take()
                .and_then(|v| resolve_env_opt(&v));
        }
    }
}

fn validate_repo_url(url: &str, repo_name: &str) -> anyhow::Result<()> {
    let is_scp_ssh = url.contains('@') && url.contains(':') && !url.contains("://");
    if is_scp_ssh {
        return Ok(());
    }
    let allowed = ["https://", "http://", "git://", "ssh://"];
    if !allowed.iter().any(|s| url.starts_with(s)) {
        anyhow::bail!(
            "repo '{}': unsupported URL scheme in '{}'. Allowed: https, http, git, ssh, or git@ SCP-style",
            repo_name,
            url
        );
    }
    Ok(())
}

fn validate_git_ref(git_ref: &str, repo_name: &str) -> anyhow::Result<()> {
    if git_ref.is_empty() {
        anyhow::bail!("repo '{}': git ref cannot be empty", repo_name);
    }
    if git_ref.contains("..") || git_ref.contains('\0') {
        anyhow::bail!(
            "repo '{}': git ref '{}' contains forbidden sequence",
            repo_name,
            git_ref
        );
    }
    if !git_ref
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '.' | '-'))
    {
        anyhow::bail!("repo '{}': invalid git ref '{}'", repo_name, git_ref);
    }
    Ok(())
}

/// If the value starts with `$`, resolve from the environment.
/// `$VAR` and `${VAR}` are both supported.
/// Returns the resolved value, or None if the env var is not set.
/// Literal values (not starting with `$`) are returned as-is.
fn resolve_env_opt(value: &str) -> Option<String> {
    if let Some(var) = value.strip_prefix('$') {
        let var_name = var.trim_start_matches('{').trim_end_matches('}');
        match std::env::var(var_name) {
            Ok(v) => Some(v),
            Err(_) => {
                tracing::error!(var = %var_name, "env var not set, auth field will be empty");
                None
            }
        }
    } else {
        Some(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config() {
        let toml = r#"
[server]
bind = "0.0.0.0:8080"

[indexer]
data_dir = "/tmp/data"
max_concurrent_indexers = 2

[[repos]]
name = "test"
url = "https://github.com/test/test.git"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.server.bind, "0.0.0.0:8080");
        assert_eq!(config.indexer.data_dir, "/tmp/data");
        assert_eq!(config.repos.len(), 1);
        assert_eq!(config.repos[0].git_ref, None); // default: auto-detect
    }

    #[test]
    fn reject_duplicate_repo_names() {
        let toml = r#"
[server]
bind = "0.0.0.0:8080"
[indexer]
data_dir = "/tmp"
[[repos]]
name = "dup"
url = "https://a.git"
[[repos]]
name = "dup"
url = "https://b.git"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn resolve_env_var_literal() {
        assert_eq!(resolve_env_opt("my-token"), Some("my-token".into()));
    }

    #[test]
    fn resolve_env_var_from_env() {
        std::env::set_var("houndr_TEST_TOKEN", "secret123");
        assert_eq!(
            resolve_env_opt("$houndr_TEST_TOKEN"),
            Some("secret123".into())
        );
        assert_eq!(
            resolve_env_opt("${houndr_TEST_TOKEN}"),
            Some("secret123".into())
        );
        std::env::remove_var("houndr_TEST_TOKEN");
    }

    #[test]
    fn resolve_env_var_missing() {
        assert_eq!(resolve_env_opt("$houndr_NONEXISTENT_VAR_12345"), None);
    }

    #[test]
    fn deny_unknown_fields() {
        let toml = r#"
[server]
bind = "0.0.0.0:8080"
cor_origins = ["http://localhost"]

[indexer]
data_dir = "/tmp"

[[repos]]
name = "test"
url = "https://github.com/test/test.git"
"#;
        let result: Result<Config, _> = toml::from_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn reject_zero_poll_interval() {
        let toml = r#"
[server]
[indexer]
poll_interval_secs = 0
[[repos]]
name = "test"
url = "https://github.com/test/test.git"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn reject_zero_max_file_size() {
        let toml = r#"
[server]
[indexer]
max_file_size = 0
[[repos]]
name = "test"
url = "https://github.com/test/test.git"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn reject_zero_max_request_bytes() {
        let toml = r#"
[server]
max_request_bytes = 0
[indexer]
[[repos]]
name = "test"
url = "https://github.com/test/test.git"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_url_https_ok() {
        assert!(validate_repo_url("https://github.com/test/test.git", "test").is_ok());
    }

    #[test]
    fn validate_url_scp_ok() {
        assert!(validate_repo_url("git@github.com:test/test.git", "test").is_ok());
    }

    #[test]
    fn validate_url_file_rejected() {
        assert!(validate_repo_url("file:///etc/passwd", "test").is_err());
    }

    #[test]
    fn validate_ref_main_ok() {
        assert!(validate_git_ref("main", "test").is_ok());
    }

    #[test]
    fn validate_ref_slash_ok() {
        assert!(validate_git_ref("release/v1.0", "test").is_ok());
    }

    #[test]
    fn validate_ref_traversal_rejected() {
        assert!(validate_git_ref("../etc", "test").is_err());
    }

    #[test]
    fn validate_ref_null_byte_rejected() {
        assert!(validate_git_ref("main\0bad", "test").is_err());
    }
}
