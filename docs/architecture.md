# Architecture

This document describes the internal architecture, design decisions, and optimization techniques in houndr.

## Overview

houndr is a trigram-based code search engine inspired by Google's [Code Search](https://swtch.com/~rsc/regexp/regexp4.html) paper and [Hound](https://github.com/hound-search/hound). It is structured as a Rust workspace with three crates:

```txt
┌──────────────────────────────────────────────────┐
│                 houndr-server                    │
│         HTTP API · Web UI · App State            │
│                                                  │
│  ┌──────────────┐  ┌──────────────────────────┐  │
│  │   Axum       │  │   Background Watcher     │  │
│  │   Router     │  │   (poll loop)            │  │
│  └──────┬───────┘  └───────────┬──────────────┘  │
└─────────┼──────────────────────┼─────────────────┘
          │                      │
          ▼                      ▼
┌──────────────────┐   ┌───────────────────────────┐
│   houndr-index   │   │       houndr-repo         │
│                  │   │                           │
│  IndexBuilder    │◄──│  pipeline::index_repo()   │
│  IndexReader     │   │  vcs::GitRepo             │
│  QueryPlan       │   │  config::Config           │
│  Trigram engine  │   │                           │
└──────────────────┘   └───────────────────────────┘
```

---

## Design Decisions

### Why trigrams?

Trigram indexing (splitting every file into overlapping 3-byte windows) gives us **substring search without full-text scans**. A query like `readFile` produces trigrams `rea`, `ead`, `adF`, `dFi`, `Fil`, `ile` - each maps to a bitmap of documents containing that trigram. Intersecting the bitmaps narrows candidates to a small set before any content scanning happens. This makes searches fast regardless of corpus size.

We chose trigrams over suffix arrays or inverted word indexes because:

- **No tokenization required** - works on any byte stream, not just natural language
- **Substring matching** - supports queries that don't align to word boundaries (e.g. `readFi`)
- **Simple to build** - linear scan to extract, straightforward to persist
- **Proven at scale** - Google Code Search and Hound both use this approach

### Why Rust?

The original Hound is written in Go. We chose Rust for:

- **Zero-cost abstractions** - memory-mapped I/O, zero-copy reads, no GC pauses
- **Fearless concurrency** - `Arc<IndexReader>` shared across threads with compile-time safety
- **Single binary deployment** - no runtime dependencies

### Why embedded content?

File content is stored directly inside the `.idx` file rather than in a separate content store. This means:

- **Single file per repo** - simpler deployment, atomic updates via rename
- **Zero-copy reads** - content is accessed via mmap slice, never copied into heap memory
- **No filesystem overhead** - avoids per-file stat/open/read for thousands of small files

The tradeoff is larger index files, but for code search this is acceptable - a 100MB codebase produces roughly a 120MB index.

### Why bare git clones?

Repos are cloned as bare repositories (`git clone --bare`). This avoids materializing a full working directory on disk - file content is read directly from git blobs during indexing. Benefits:

- **~50% less disk usage** - no working copy
- **Faster fetches** - less I/O on updates
- **Simpler cleanup** - single directory per repo

### Why polling over webhooks?

The watcher uses a configurable poll interval (default 60s) rather than webhook-triggered indexing. This keeps deployment simple - no public endpoint needed, no webhook secrets to manage, works behind firewalls. The poll is cheap: `git fetch` only transfers new objects, and the manifest check skips re-indexing entirely when HEAD hasn't changed.

### Why LRU cache with TTL?

Search results are cached as serialized JSON strings in an LRU cache. The TTL (default 300s) ensures stale results are evicted after re-indexing. We cache the JSON string rather than the deserialized struct to avoid re-serialization on cache hits - the response is sent directly as bytes.

---

## Crates

### houndr-index

The core search engine. Provides trigram extraction, index building, disk I/O, and query execution. This crate has **no Git or HTTP dependencies** and can be used standalone as a library.

**Public API:**

```rust
pub use builder::IndexBuilder;   // Build indexes from documents
pub use reader::IndexReader;     // Memory-mapped index reader
pub use query::{QueryPlan, execute_search, SearchResult, FileMatch, MatchBlock, LineMatch};
pub use trigram::Trigram;         // 3-byte trigram primitive
```

**Key types:**

| Type | File | Purpose |
|------|------|---------|
| `Trigram` | `trigram.rs` | 3-byte sequence packed into `u32`. Methods: `new()`, `extract()`, `extract_unique()` |
| `IndexBuilder` | `builder.rs` | Collects documents, extracts trigrams in parallel (rayon fold/reduce), produces `BuiltIndex` |
| `write_index()` | `writer.rs` | Serializes `BuiltIndex` to binary `.idx` file in 9 phases with atomic rename |
| `IndexReader` | `reader.rs` | Memory-maps `.idx` file, validates checksum, provides zero-copy lookups |
| `QueryPlan` | `query.rs` | Parses query into `Literal` or `Regex` variant, extracts trigrams for index lookup |
| `execute_search()` | `query.rs` | Intersects posting lists, scans candidates in parallel, returns grouped matches |

**Search algorithm:**

```
Query: "readFile"
  │
  ▼
1. Extract trigrams: [rea, ead, adF, dFi, Fil, ile]
  │
  ▼
2. Look up posting lists (RoaringBitmap per trigram)
   rea → {0, 3, 7, 12, 45}
   ead → {0, 3, 12, 45, 99}
   adF → {0, 12, 45}
   ...
  │
  ▼
3. Intersect bitmaps (smallest-first, early termination)
   candidates → {0, 12, 45}
  │
  ▼
4. Parallel content scan (rayon) with AtomicUsize counter
   For each candidate: scan lines, find matches, record ranges
   First max_results files: build full match blocks with context
   Remaining files: count matches only (for accurate totals)
  │
  ▼
5. Group matches into context blocks (surrounding lines merged)
   Return SearchResult { repo, files, total_file_count, total_match_count }
```

### houndr-repo

Git repository management, configuration, and the indexing pipeline.

**Key types:**

| Type | File | Purpose |
|------|------|---------|
| `Config` | `config.rs` | TOML config: `ServerConfig`, `IndexerConfig`, `CacheConfig`, `Vec<RepoConfig>` |
| `GitRepo` | `vcs.rs` | Git operations via `git2`: clone, fetch, tree walk, blob read |
| `index_repo()` | `pipeline.rs` | Full indexing pipeline: fetch → diff manifest → build → write → return reader |

**Authentication priority:**

1. `auth_token` - HTTPS token (GitHub PAT, GitLab PAT). Uses `git2::Cred::userpass_plaintext`.
2. `ssh_key` - SSH private key content in memory. Uses `git2::Cred::ssh_key_from_memory`.
3. `ssh_key_path` - SSH key file path. Uses `git2::Cred::ssh_key`.

All auth fields support `$VAR` / `${VAR}` env-var resolution at config load time.

**Data layout on disk:**

```
data/
  repos/<name>/         bare git clone
  indexes/<name>.idx    compiled trigram index
  manifests/<name>.json path → blob OID manifest (for incremental indexing)
```

### houndr-server

HTTP server and web UI. Built on Axum with Tower middleware.

**Shared state (`AppState`):**

```rust
pub struct AppState {
    pub config: Config,
    pub readers: RwLock<Vec<Arc<IndexReader>>>,        // one per indexed repo
    pub cache: RwLock<LruCache<String, CachedResult>>, // search result cache
    pub repo_statuses: RwLock<HashMap<String, RepoStatus>>,
    pub resolved_refs: RwLock<HashMap<String, String>>, // repo → git ref
    pub last_watcher_heartbeat: RwLock<Option<Instant>>, // for healthz staleness
    pub poll_interval_secs: u64,
}
```

**Repo lifecycle:**

```
Pending → Indexing → Ready
                  ↘ Failed { error }
```

Failed repos retain their previous `IndexReader` - search continues against the last successful index.

---

## API Specification

### `GET /api/v1/search`

JSON search across indexed repositories.

**Query parameters:**

| Param | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `q` | string | yes | - | Search query (literal or regex). Minimum 3 characters for trigram extraction. |
| `repos` | string | no | all | Comma-separated repo names to search. |
| `files` | string | no | all | Glob pattern to filter file paths (e.g. `*.rs`, `src/**/*.ts`). |
| `i` | bool | no | `false` | Case-insensitive matching. |
| `regex` | bool | no | `false` | Treat `q` as a regular expression. |
| `max` | int | no | `10000` | Maximum file matches to return per repo. 0 = server default. The response always includes accurate total counts regardless of this limit. |

**Response:**

```json
{
  "results": [
    {
      "repo": "my-project",
      "url": "https://github.com/user/my-project.git",
      "git_ref": "main",
      "files": [
        {
          "path": "src/main.rs",
          "match_count": 1,
          "blocks": [
            {
              "lines": [
                {
                  "line_number": 10,
                  "line": "fn readFile(path: &str) -> String {",
                  "match_ranges": [[3, 11]]
                }
              ]
            }
          ]
        }
      ],
      "total_file_count": 42,
      "total_match_count": 128
    }
  ],
  "duration_ms": 1.23,
  "total_files": 42,
  "total_matches": 128,
  "truncated": false
}
```

**Cache behavior:** Results are cached as JSON strings keyed by `q+repos+files+i+regex+max`. Cache hits skip query planning and search entirely. TTL is configurable (default 300s). Cache is invalidated implicitly - entries expire, and the LRU eviction handles memory pressure.

### `GET /api/v1/search/stream`

SSE streaming search. Same parameters as `/api/v1/search`.

**Event stream:**

```
event: result
data: {"repo":"my-project","files":[...],"total_file_count":42,"total_match_count":128}

event: result
data: {"repo":"other-project","files":[...],"total_file_count":5,"total_match_count":12}

event: done
data: {"duration_ms":2.45,"total_files":47}
```

Each `result` event contains matches for a single repo, including accurate `total_file_count` and `total_match_count` (which may exceed the number of files in the `files` array when the `max` limit is reached). Results arrive as each repo's search completes - useful for large deployments where some repos are much larger than others.

### `GET /api/v1/repos`

List all indexed repositories.

**Response:**

```json
[
  {
    "name": "my-project",
    "doc_count": 1234,
    "trigram_count": 98765
  }
]
```

### `GET /api/v1/status`

Per-repo indexing status.

**Response:**

```json
{
  "my-project": { "status": "ready" },
  "other-project": { "status": "indexing" },
  "broken-repo": { "status": "failed", "error": "fetch failed: authentication required" }
}
```

Possible statuses: `pending`, `indexing`, `ready`, `failed`.

### `GET /healthz`

Health check endpoint.

**Response:**

```json
{
  "status": "ready",
  "repos_indexed": 3,
  "total_docs": 4567
}
```

`status` is `"ready"` when at least one repo is indexed, `"initializing"` otherwise.

---

## Binary Index Format

The `.idx` file is a custom binary format designed for memory-mapped random access with naturally aligned fields and inline small postings.

```
┌──────────────────────────────────────────────────────────────┐
│ HEADER (64 bytes)                                            │
│   magic:           "HNDR" (4 bytes)                          │
│   version:         u32 = 3                                   │
│   doc_count:       u32                                       │
│   trigram_count:   u32                                       │
│   doc_table_off:   u64                                       │
│   path_data_off:   u64                                       │
│   trigram_idx_off: u64                                       │
│   posting_off:     u64                                       │
│   content_off:     u64                                       │
│   reserved:        4 bytes                                   │
├──────────────────────────────────────────────────────────────┤
│ DOC TABLE (24 bytes × doc_count, naturally aligned)          │
│   path_offset:     u32 (offset 0)                            │
│   path_len:        u32 (offset 4)                            │
│   content_offset:  u64 (offset 8)                            │
│   content_len:     u64 (offset 16)                           │
├──────────────────────────────────────────────────────────────┤
│ PATH STRINGS                                                 │
│   concatenated UTF-8 paths                                   │
├──────────────────────────────────────────────────────────────┤
│ TRIGRAM INDEX (16 bytes × count, sorted by trigram value)    │
│   u32 word 0:                                                │
│     bits [0:23]  = trigram value                             │
│     bit 24       = inline flag (1=inline, 0=offset)          │
│     bits [25:26] = inline count - 1 (0..2 → 1..3 doc IDs)    │
│     bits [27:31] = reserved                                  │
│   bytes 4-15: 12-byte payload                                │
│     Inline:  up to 3 × u32 doc IDs (unused slots zeroed)     │
│     Offset:  posting_offset(u64) + posting_len(u32)          │
├──────────────────────────────────────────────────────────────┤
│ POSTING DATA                                                 │
│   serialized RoaringBitmaps (only for non-inline entries)    │
├──────────────────────────────────────────────────────────────┤
│ CONTENT DATA                                                 │
│   concatenated raw file contents                             │
├──────────────────────────────────────────────────────────────┤
│ FOOTER (8 bytes)                                             │
│   checksum:        u64 (xxhash3)                             │
└──────────────────────────────────────────────────────────────┘
```

**Write process (9 phases):**

1. Write placeholder header (64 zero bytes)
2. Write doc table entries (path offset, path length, content offset, content length)
3. Write concatenated path strings
4. Serialize RoaringBitmaps for non-inline entries (trigrams with >3 docs)
5. Write trigram index - inline entries pack doc IDs directly, offset entries reference posting data
6. Write posting data (serialized bitmaps, only for non-inline entries)
7. Write content data (raw file bytes)
8. Seek back and write real header with computed offsets
9. Compute streaming xxhash3 checksum, append as footer, atomic rename

**Read process:**

1. Memory-map the file (`Mmap::map`)
2. Apply segment-specific `madvise` hints: `WillNeed` for the header, `Random` for the trigram index and posting data (binary search / sparse access), `Sequential` for content data (streamed reads)
3. Validate magic, version, and checksum
4. Parse header offsets and validate they don't exceed file size
5. All subsequent reads use checked arithmetic and zero-copy slices into the mmap

**Trigram lookup:** Binary search over the sorted trigram index section (masking lower 24 bits for comparison). O(log n) per trigram. Inline postings (≤3 docs) are read directly from the trigram entry. Larger postings are deserialized on demand from the posting section using unchecked deserialization (file integrity is already validated by the xxhash3 checksum).

**Search intersection:** Trigram lookups return either materialized inline bitmaps or raw serialized slices. These are sorted by estimated size (smallest first) and intersected lazily with early termination when the intermediate result becomes empty. Serialized slices are intersected via `intersection_with_serialized_unchecked`, which only deserializes the internal Roaring containers that overlap with the current result bitmap, skipping non-overlapping containers entirely.

---

## Optimization Techniques

### Memory-mapped I/O

`IndexReader` uses `memmap2::Mmap` to memory-map index files with segment-specific `madvise` hints. The trigram index and posting sections use `Advice::Random` (binary search and sparse access), while the content section uses `Advice::Sequential` (streamed reads). This means:

- **Zero-copy reads** - file content and posting data are accessed as byte slices directly from the OS page cache
- **Lazy loading** - only pages actually accessed are loaded into physical memory
- **No heap allocation** - document content is returned as `&[u8]` slices into the mmap
- **OS-managed eviction** - the kernel handles page eviction under memory pressure
- **Optimal prefetching** - the OS read-ahead strategy matches each section's access pattern

### Inline small postings

Trigrams appearing in ≤3 documents store their doc IDs directly in the 12-byte trigram index entry payload, avoiding a pointer chase to the posting section. This benefits the ~30-50% of trigrams that are rare, eliminating separate bitmap serialization, deserialization, and a cache-unfriendly random read.

### Smallest-first bitmap intersection

When intersecting posting lists, bitmaps are sorted by cardinality (smallest first). The smallest bitmap produces the fewest candidates, and each subsequent intersection can only shrink the result. If the intermediate result becomes empty, the loop terminates early. Serialized posting slices use `intersection_with_serialized_unchecked`, which partially deserializes only the internal Roaring containers that overlap with the current result - non-overlapping containers are skipped via `Seek`, reducing allocations and copies.

```
Trigram A: 50,000 documents
Trigram B: 200 documents     ← start here
Trigram C: 10,000 documents

Intersection order: B ∩ A ∩ C
After B ∩ A: maybe 150 docs (fast - B is small)
After ∩ C: maybe 120 docs
```

### Parallel trigram extraction (rayon fold/reduce)

Index building uses rayon's `par_iter().fold().reduce()` pattern:

- **Fold phase**: each thread accumulates trigrams from its document subset into a thread-local `FxHashMap<Trigram, RoaringBitmap>`
- **Reduce phase**: thread-local maps are merged via bitmap union

This avoids lock contention - no shared mutable state during extraction.

### Parallel query execution

Two levels of parallelism during search:

1. **Cross-repo** (rayon `par_iter` in `api.rs`): all matching repos are searched concurrently
2. **Within-repo** (rayon `par_iter` in `query.rs`): candidate documents are verified in parallel. Full file details (match blocks with line content) are built for the first `max_results` files. Remaining matches are counted but not materialized, keeping response size bounded while providing accurate totals.

### FxHash for hot-path hashing

`rustc_hash::FxHashMap` and `FxHashSet` are used for trigram deduplication and accumulation. FxHash is a fast, non-cryptographic hash optimized for small integer keys - ideal for `u32`-packed trigrams.

### xxhash3 for integrity

Index file integrity is verified via xxhash3 (streaming 64-bit hash). xxhash3 is chosen for its speed - checksumming a 100MB index takes ~10ms, which is negligible compared to index build time.

### LRU cache with TTL

Search results are cached as pre-serialized JSON strings. Cache key includes all query parameters. Benefits:

- **Skip query planning and search** on cache hit
- **No re-serialization** - cached JSON is returned as-is
- **Bounded memory** - LRU eviction keeps the cache at `max_entries`
- **Freshness** - TTL ensures stale results are evicted after re-indexing

### Incremental indexing with manifest tracking

On each poll cycle, the pipeline:

1. Fetches latest refs - if HEAD is unchanged, returns the existing `IndexReader` (cheapest path)
2. If HEAD changed, walks the new tree to build a manifest (`path → blob OID`)
3. Compares against the previous manifest
4. For **unchanged files** (same OID): reuses content directly from the old index via mmap (zero-copy)
5. For **changed/new files**: reads the blob from git
6. Deleted files are simply omitted from the new index

This means a commit that changes 2 files in a 10,000-file repo only reads 2 blobs from git - the other 9,998 files are copied from the mmap of the previous index.

### Binary detection

Files are checked for binary content by scanning the first 8KB for null bytes. Binary files are excluded from indexing to avoid polluting trigram posting lists with noise.

### Atomic index writes

Index files are written to a `.tmp` file first, then atomically renamed. This ensures readers never see a partially-written index - the old mmap remains valid until the rename completes.

---

## Data Flow

### Startup & Indexing

```
1. Load config.toml
2. Create AppState (all repos → Pending)
3. Spawn background watcher task
4. Start HTTP server (immediately accepting requests)

Background watcher:
  for each repo in config.repos:
    set status → Indexing
    │
    ├─ First run: bare git clone
    │  Subsequent: git fetch (only new objects)
    │
    ├─ HEAD unchanged? → reuse existing IndexReader, set → Ready
    │
    ├─ HEAD changed:
    │   walk tree manifest (no blob reads)
    │   diff against previous manifest
    │   reuse unchanged files from old index (mmap zero-copy)
    │   read changed blobs from git
    │   build trigram index (parallel via rayon)
    │   write .idx to disk (atomic rename)
    │   open IndexReader (mmap)
    │   save new manifest
    │   set → Ready
    │
    └─ On error: set → Failed, keep old IndexReader

  merge new readers into AppState.readers
  invalidate cache (LRU eviction handles this naturally)
  sleep poll_interval_secs
  repeat
```

### Search Request

```
1. Parse query parameters (q, repos, files, i, regex, max)
2. Build cache key from all parameters
3. Check LRU cache → return cached JSON string directly if hit
4. Build QueryPlan:
   - Literal: extract trigrams from query string
   - Regex: parse regex, extract literal fragments, extract trigrams
5. Acquire read lock on AppState.readers
6. Filter readers by repo name (if repos param specified)
7. Search all matching repos in parallel (rayon par_iter):
   a. Look up posting list for each query trigram (binary search)
   b. Intersect bitmaps (smallest-first, early termination)
   c. For each candidate doc (parallel):
      - Scan content for actual matches
      - First max files: build full match blocks with context
      - Remaining files: count only (for accurate totals)
   d. Group matches into context blocks
8. Attach repo URL and git_ref from config
9. Serialize to JSON, store in cache
10. Return response
```

### Shutdown

Graceful shutdown uses a three-layer cancellation strategy:

1. **First Ctrl+C / SIGTERM:** Axum's graceful shutdown stops accepting new connections. A `CancellationToken` is cancelled, which bridges to a sync `AtomicBool` flag shared with all blocking indexing threads.
2. **Cooperative cancellation:** Git fetch operations check the flag in their `transfer_progress` callback and return `false` to abort the transfer. Tree walks and blob reads check the flag between iterations. `index_repo` checks between each phase (clone, fetch, build, write). `run_indexing` stops spawning new tasks and aborts pending handles.
3. **Second Ctrl+C:** Force-quits the process via `std::process::exit(1)` - a backstop for any operation that doesn't check the flag promptly.

---

## Web UI

HTML template (`templates/index.html`) with external CSS and JavaScript (`static/app.css`, `static/app.js`). All static assets are embedded at compile time via `include_str!` and served from `/static/` routes with cache-control headers. No build step, no dependencies, no bundler.

**Features:**

- Debounced search input (300ms delay)
- SSE streaming - search uses the `/api/v1/search/stream` endpoint to render repos progressively as each completes, keeping the page responsive even for large result sets. Per-repo file batching (first 30 files shown, rest via "Show more" button) keeps initial rendering fast
- Results grouped by repo, then by file, with syntax-highlighted match ranges
- Collapsible file blocks (click header to toggle, per-repo expand/collapse all)
- File name and line number links point to the source repo (GitHub, GitLab, etc.) with automatic SCP-style SSH URL to HTTPS conversion
- Indexing status indicator in the header bar - shows progress during boot, fades out once ready
- Dark theme with custom selection colors and SVG favicon
- Search parameters are synced to the browser URL for shareability. Opening a URL with `?q=...` parameters restores the search automatically. File and line number links use `encodeURIComponent` on each path segment to prevent XSS via crafted file paths.

**Status bar behavior:**

- Polls `/api/v1/status` every 3 seconds
- If all repos are already ready on first poll (page refresh), the status bar never appears
- Shows during active indexing: "2/5 repos ready · 3 indexing"
- Shows failures persistently: "4/5 repos ready · 1 failed"
- Fades out 3 seconds after all repos reach ready state
- Stops polling once all repos are in a terminal state (ready or failed)

---

## CLI Integration

The `houndr-index` crate is a standalone library with no server or HTTP dependencies. It can be used to build CLI tools that create and search indexes directly:

```rust
use houndr_index::{IndexBuilder, IndexReader};
use houndr_index::writer::write_index;
use houndr_index::query::{QueryPlan, execute_search};
use std::path::Path;

// Build an index
let mut builder = IndexBuilder::new();
builder.add_doc("src/main.rs".into(), content);
let built = builder.build();
write_index(&built, Path::new("index.idx")).unwrap();

// Search an index
let reader = IndexReader::open(Path::new("index.idx"), "my-repo".into()).unwrap();
let plan = QueryPlan::new("searchTerm", false, false).unwrap();
let result = execute_search(&reader, &plan, 50, None, false);
```

The `houndr-repo` crate can also be used standalone to clone repos and build indexes without running a server - useful for CI pipelines or offline index generation.

---

## Resource Requirements

- **Memory:** ~120% of total index size. Indexes are memory-mapped, so the OS page cache manages physical memory usage.
- **Disk:** bare git clones + compiled indexes. Indexes are roughly 1.2× the raw source size.
- **CPU:** indexing is CPU-intensive (trigram extraction + parallel rayon); search is fast (binary search + bitmap intersection).
- **Recommended:** 2+ cores, 2GB+ RAM for 10-20 repos totaling ~500MB of source code.

---

## Configuration Reference

All settings live in `config.toml`. See the [config file](../config.toml) for the full reference with inline comments.

### `[server]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `bind` | string | `"127.0.0.1:6080"` | Address and port to listen on |
| `timeout_secs` | u64 | `30` | Request timeout in seconds. Returns 504 on timeout. |
| `cors_origins` | string[] | `[]` (permissive) | Allowed CORS origins. Empty = allow all. |
| `rate_limit_rps` | u64 | `0` (unlimited) | Max requests per second per IP. 0 disables rate limiting. |
| `max_request_bytes` | usize | `1048576` | Max request body size in bytes. |
| `max_search_results` | usize | `10000` | Max file matches per repo per query. Caps the `max` query parameter. |

### `[indexer]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `data_dir` | string | `"data"` | Directory for repos, indexes, and manifests |
| `max_concurrent_indexers` | usize | `4` | Max repos indexed in parallel (semaphore) |
| `poll_interval_secs` | u64 | `60` | Seconds between re-index polls |
| `max_file_size` | usize | `1048576` | Skip files larger than this (bytes) |
| `exclude_patterns` | string[] | `[]` | Glob patterns to exclude from indexing |
| `index_timeout_secs` | u64 | `300` | Per-repo indexing timeout in seconds |

### `[cache]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `max_entries` | usize | `1000` | Max cached search results (LRU eviction) |
| `ttl_secs` | u64 | `300` | Cache entry time-to-live in seconds |

### `[[repos]]`

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `name` | string | yes | - | Unique identifier for the repo |
| `url` | string | yes | - | Git clone URL (HTTPS or SSH) |
| `ref` | string | no | auto-detect | Branch or tag to index. If omitted, uses the remote's default branch (HEAD). |
| `auth_token` | string | no | - | HTTPS token (PAT) |
| `ssh_key` | string | no | - | SSH private key content (PEM) |
| `ssh_key_path` | string | no | - | Path to SSH private key file |
| `ssh_key_passphrase` | string | no | - | Passphrase for encrypted SSH keys |

Auth fields support `$VAR` and `${VAR}` syntax to read values from environment variables at startup.

**Validation rules:**

- All config structs use `deny_unknown_fields` to catch typos (e.g. `cor_origins` instead of `cors_origins`)
- Numeric fields (`poll_interval_secs`, `max_file_size`, `max_request_bytes`, etc.) must be > 0
- Repo URLs must use `https://`, `http://`, `git://`, `ssh://`, or git@ SCP-style - `file://` is rejected to prevent SSRF
- Git refs are validated for forbidden sequences (`..`, null bytes) and restricted to alphanumeric, `/`, `_`, `.`, `-`
- Symlinks (git filemode 120000) are filtered out during tree walks

---

## Middleware

| Layer | Source | Purpose |
|-------|--------|---------|
| `CorsLayer` | `tower-http` | CORS handling. Permissive by default, restrictable via `cors_origins`. |
| `CompressionLayer` | `tower-http` | Gzip response compression. |
| `TimeoutLayer` | `tower-http` | Request timeout. Returns `504 Gateway Timeout`. Configurable via `timeout_secs`. |
| Security headers | `axum::middleware` | Sets `X-Content-Type-Options`, `X-Frame-Options`, `Referrer-Policy`, `X-XSS-Protection`, `Content-Security-Policy` (`default-src 'self'; script-src 'self'; style-src 'self'` - no inline). |
| `DefaultBodyLimit` | `axum` | Caps request body size. Configurable via `max_request_bytes`. |
| Rate limiter | `governor` | Per-IP rate limiting with periodic cleanup task (retains recent entries every 60s). Returns 429 when exceeded. Configurable via `rate_limit_rps`. |
