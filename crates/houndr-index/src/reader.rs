use memmap2::Mmap;
use roaring::RoaringBitmap;
use std::fs;
use std::path::Path;
use xxhash_rust::xxh3::xxh3_64;

use crate::posting::intersect_with_serialized;
use crate::trigram::Trigram;
use crate::writer::{DOC_ENTRY_SIZE, TRIGRAM_ENTRY_SIZE};

/// Intermediate result from trigram lookup — avoids eager deserialization.
enum RawPosting<'a> {
    /// Small posting stored inline (≤3 doc IDs), already materialized.
    Inline(RoaringBitmap),
    /// Serialized Roaring bitmap slice from the posting section.
    Serialized(&'a [u8]),
}

impl RawPosting<'_> {
    fn into_bitmap(self) -> RoaringBitmap {
        match self {
            RawPosting::Inline(b) => b,
            RawPosting::Serialized(data) => {
                // The full file is validated via xxhash3 checksum at open time,
                // so per-bitmap validation is redundant.
                RoaringBitmap::deserialize_unchecked_from(data).unwrap_or_default()
            }
        }
    }

    fn serialized_len(&self) -> usize {
        match self {
            RawPosting::Inline(b) => b.serialized_size(),
            RawPosting::Serialized(data) => data.len(),
        }
    }
}

/// Errors that can occur when opening or reading an index file.
#[derive(Debug, thiserror::Error)]
pub enum ReaderError {
    /// Underlying I/O error (file not found, permission denied, etc.).
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// The index file is malformed (bad magic, version, or truncated).
    #[error("invalid index: {0}")]
    Invalid(String),
    /// The xxhash3 checksum in the footer does not match the file contents.
    #[error("checksum mismatch")]
    Checksum,
}

/// Read a little-endian u32 from the mmap at the given offset, with bounds check.
fn read_u32(mmap: &[u8], off: usize) -> Result<u32, ReaderError> {
    mmap.get(off..off + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| ReaderError::Invalid(format!("u32 read out of bounds at offset {}", off)))
}

/// Read a little-endian u64 from the mmap at the given offset, with bounds check.
fn read_u64(mmap: &[u8], off: usize) -> Result<u64, ReaderError> {
    mmap.get(off..off + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| ReaderError::Invalid(format!("u64 read out of bounds at offset {}", off)))
}

/// Get a byte slice from the mmap with bounds check. Returns None if out of bounds.
fn slice(mmap: &[u8], off: usize, len: usize) -> Option<&[u8]> {
    mmap.get(off..off.checked_add(len)?)
}

/// Memory-mapped index reader.
pub struct IndexReader {
    mmap: Mmap,
    doc_count: u32,
    trigram_count: u32,
    doc_table_off: u64,
    path_data_off: u64,
    trigram_idx_off: u64,
    posting_off: u64,
    content_off: u64,
    /// The name of the repo this index belongs to.
    pub repo_name: String,
}

impl IndexReader {
    /// Open and validate a memory-mapped index file.
    pub fn open(path: &Path, repo_name: String) -> Result<Self, ReaderError> {
        let file = fs::File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };

        // Segment-specific madvise hints are applied after parsing the header below.

        // Validate minimum size: header(64) + footer(8)
        if mmap.len() < 72 {
            return Err(ReaderError::Invalid("file too small".into()));
        }

        // Validate magic
        if &mmap[0..4] != b"HNDR" {
            return Err(ReaderError::Invalid("bad magic".into()));
        }

        // Validate version
        let version = read_u32(&mmap, 4)?;
        if version != 3 {
            return Err(ReaderError::Invalid(format!(
                "unsupported version {}",
                version
            )));
        }

        // Validate checksum
        let content_len = mmap.len() - 8;
        let stored_checksum = read_u64(&mmap, content_len)?;
        let computed_checksum = xxh3_64(&mmap[..content_len]);
        if stored_checksum != computed_checksum {
            return Err(ReaderError::Checksum);
        }

        let doc_count = read_u32(&mmap, 8)?;
        let trigram_count = read_u32(&mmap, 12)?;
        let doc_table_off = read_u64(&mmap, 16)?;
        let path_data_off = read_u64(&mmap, 24)?;
        let trigram_idx_off = read_u64(&mmap, 32)?;
        let posting_off = read_u64(&mmap, 40)?;
        let content_off = read_u64(&mmap, 48)?;

        // Validate header offsets don't exceed file size
        let file_len = mmap.len() as u64;
        for (name, off) in [
            ("doc_table", doc_table_off),
            ("path_data", path_data_off),
            ("trigram_idx", trigram_idx_off),
            ("posting", posting_off),
            ("content", content_off),
        ] {
            if off > file_len {
                return Err(ReaderError::Invalid(format!(
                    "{} offset {} exceeds file size {}",
                    name, off, file_len
                )));
            }
        }

        // Segment-specific madvise hints
        #[cfg(unix)]
        {
            use memmap2::Advice;
            // Header: likely re-read, prefetch it
            let _ = mmap.advise_range(Advice::WillNeed, 0, 64);
            // Trigram index: binary search → random access
            let tri_idx_size = (trigram_count as usize) * TRIGRAM_ENTRY_SIZE;
            let _ = mmap.advise_range(Advice::Random, trigram_idx_off as usize, tri_idx_size);
            // Posting data: sparse access by offset
            let posting_size = (content_off - posting_off) as usize;
            let _ = mmap.advise_range(Advice::Random, posting_off as usize, posting_size);
            // Content data: typically streamed sequentially
            let content_size = file_len.saturating_sub(content_off + 8) as usize;
            let _ = mmap.advise_range(Advice::Sequential, content_off as usize, content_size);
        }

        Ok(Self {
            mmap,
            doc_count,
            trigram_count,
            doc_table_off,
            path_data_off,
            trigram_idx_off,
            posting_off,
            content_off,
            repo_name,
        })
    }

    /// Number of documents in this index.
    pub fn doc_count(&self) -> u32 {
        self.doc_count
    }

    /// Number of unique trigrams in this index.
    pub fn trigram_count(&self) -> u32 {
        self.trigram_count
    }

    /// Get the file path for a given doc ID.
    pub fn doc_path(&self, doc_id: u32) -> Option<&str> {
        if doc_id >= self.doc_count {
            return None;
        }
        let entry_off = (doc_id as usize)
            .checked_mul(DOC_ENTRY_SIZE)
            .and_then(|v| v.checked_add(self.doc_table_off as usize))?;
        let path_offset = read_u32(&self.mmap, entry_off).ok()? as usize;
        let path_len = read_u32(&self.mmap, entry_off + 4).ok()? as usize;

        let abs_off = (self.path_data_off as usize).checked_add(path_offset)?;
        let data = slice(&self.mmap, abs_off, path_len)?;
        std::str::from_utf8(data).ok()
    }

    /// Get the file content for a given doc ID — zero-copy from mmap.
    pub fn doc_content(&self, doc_id: u32) -> Option<&[u8]> {
        if doc_id >= self.doc_count {
            return None;
        }
        let entry_off = (doc_id as usize)
            .checked_mul(DOC_ENTRY_SIZE)
            .and_then(|v| v.checked_add(self.doc_table_off as usize))?;
        let content_offset = read_u64(&self.mmap, entry_off + 8).ok()? as usize;
        let content_len = read_u64(&self.mmap, entry_off + 16).ok()? as usize;

        let abs_off = (self.content_off as usize).checked_add(content_offset)?;
        slice(&self.mmap, abs_off, content_len)
    }

    /// Binary search for a trigram in the trigram index.
    /// Returns the deserialized posting bitmap if found.
    pub fn lookup_trigram(&self, trigram: Trigram) -> Option<RoaringBitmap> {
        self.lookup_trigram_raw(trigram)
            .map(|raw| raw.into_bitmap())
    }

    /// Binary search returning either inline doc IDs or a raw serialized slice.
    fn lookup_trigram_raw(&self, trigram: Trigram) -> Option<RawPosting<'_>> {
        if self.trigram_count == 0 {
            return None;
        }

        let base = self.trigram_idx_off as usize;
        let mut lo = 0u32;
        let mut hi = self.trigram_count;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let off = (mid as usize)
                .checked_mul(TRIGRAM_ENTRY_SIZE)
                .and_then(|v| v.checked_add(base))?;
            let raw_val = read_u32(&self.mmap, off).ok()?;
            let stored_trigram = raw_val & 0x00FF_FFFF;

            match stored_trigram.cmp(&trigram.0) {
                std::cmp::Ordering::Equal => {
                    let inline = (raw_val >> 24) & 1 != 0;
                    if inline {
                        let count = (((raw_val >> 25) & 0x3) + 1) as usize;
                        let mut bitmap = RoaringBitmap::new();
                        for j in 0..count {
                            let doc_id = read_u32(&self.mmap, off + 4 + j * 4).ok()?;
                            bitmap.insert(doc_id);
                        }
                        return Some(RawPosting::Inline(bitmap));
                    } else {
                        let posting_offset = read_u64(&self.mmap, off + 4).ok()? as usize;
                        let posting_len = read_u32(&self.mmap, off + 12).ok()? as usize;
                        let abs_off = (self.posting_off as usize).checked_add(posting_offset)?;
                        let data = slice(&self.mmap, abs_off, posting_len)?;
                        return Some(RawPosting::Serialized(data));
                    }
                }
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }

        None
    }

    /// Search for docs containing all given trigrams.
    /// Returns doc IDs of candidate matches.
    pub fn search_trigrams(&self, trigrams: &[Trigram]) -> RoaringBitmap {
        if trigrams.is_empty() {
            return RoaringBitmap::new();
        }

        let mut raw_postings: Vec<RawPosting<'_>> = Vec::with_capacity(trigrams.len());
        for t in trigrams {
            match self.lookup_trigram_raw(*t) {
                Some(rp) => raw_postings.push(rp),
                None => return RoaringBitmap::new(), // trigram not found → no match
            }
        }

        // Sort by estimated size (smallest first for fastest intersection)
        raw_postings.sort_by_key(|rp| rp.serialized_len());

        // Separate into already-materialized bitmaps and serialized slices
        let mut serialized_slices: Vec<&[u8]> = Vec::new();
        let mut inline_bitmaps: Vec<RoaringBitmap> = Vec::new();

        for rp in raw_postings {
            match rp {
                RawPosting::Inline(b) => inline_bitmaps.push(b),
                RawPosting::Serialized(data) => serialized_slices.push(data),
            }
        }

        // Start with the smallest materialized bitmap, or deserialize the smallest slice
        let mut base = if let Some(first) = inline_bitmaps.first() {
            first.clone()
        } else if let Some(first_slice) = serialized_slices.first() {
            // File checksum validated at open time
            match RoaringBitmap::deserialize_unchecked_from(*first_slice) {
                Ok(b) => {
                    serialized_slices.remove(0);
                    b
                }
                Err(_) => return RoaringBitmap::new(),
            }
        } else {
            return RoaringBitmap::new();
        };

        // Intersect remaining inline bitmaps
        for bm in inline_bitmaps.iter().skip(1) {
            base &= bm;
            if base.is_empty() {
                return base;
            }
        }

        // Intersect remaining serialized slices
        if !serialized_slices.is_empty() {
            base = intersect_with_serialized(base, &serialized_slices);
        }

        base
    }
}

// Safety: IndexReader is safe to share across threads.
// The mmap is read-only and all methods take &self.
unsafe impl Send for IndexReader {}
unsafe impl Sync for IndexReader {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::IndexBuilder;
    use crate::writer::write_index;

    fn build_test_index(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("test.idx");
        let mut builder = IndexBuilder::new();
        builder.add_doc(
            "hello.rs".into(),
            b"fn main() { println!(\"hello\"); }".to_vec(),
        );
        builder.add_doc(
            "world.rs".into(),
            b"fn test() { assert_eq!(1, 1); }".to_vec(),
        );
        builder.add_doc("both.rs".into(), b"fn main() { test(); }".to_vec());
        let built = builder.build();
        write_index(&built, &path).unwrap();
        path
    }

    #[test]
    fn open_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = build_test_index(dir.path());
        let reader = IndexReader::open(&path, "test".into()).unwrap();
        assert_eq!(reader.doc_count(), 3);
        assert_eq!(reader.doc_path(0), Some("hello.rs"));
        assert_eq!(reader.doc_path(1), Some("world.rs"));
        assert_eq!(reader.doc_path(2), Some("both.rs"));
    }

    #[test]
    fn doc_content_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = build_test_index(dir.path());
        let reader = IndexReader::open(&path, "test".into()).unwrap();

        assert_eq!(
            reader.doc_content(0),
            Some(b"fn main() { println!(\"hello\"); }".as_slice())
        );
        assert_eq!(
            reader.doc_content(1),
            Some(b"fn test() { assert_eq!(1, 1); }".as_slice())
        );
        assert_eq!(
            reader.doc_content(2),
            Some(b"fn main() { test(); }".as_slice())
        );
        assert_eq!(reader.doc_content(3), None);
    }

    #[test]
    fn trigram_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let path = build_test_index(dir.path());
        let reader = IndexReader::open(&path, "test".into()).unwrap();

        // "fn " appears in all 3 docs
        let trigram = Trigram::new(b'f', b'n', b' ');
        let bitmap = reader.lookup_trigram(trigram).unwrap();
        assert_eq!(bitmap.len(), 3);

        // "pri" appears only in hello.rs
        let trigram = Trigram::new(b'p', b'r', b'i');
        let bitmap = reader.lookup_trigram(trigram).unwrap();
        assert!(bitmap.contains(0));
        assert!(!bitmap.contains(1));
    }

    #[test]
    fn search_trigrams_intersection() {
        let dir = tempfile::tempdir().unwrap();
        let path = build_test_index(dir.path());
        let reader = IndexReader::open(&path, "test".into()).unwrap();

        // Search for "main" — trigrams: "mai", "ain"
        let trigrams = Trigram::extract_unique(b"main");
        let result = reader.search_trigrams(&trigrams);
        // "main" appears in hello.rs and both.rs
        assert!(result.contains(0));
        assert!(result.contains(2));
        assert!(!result.contains(1));
    }

    #[test]
    fn search_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = build_test_index(dir.path());
        let reader = IndexReader::open(&path, "test".into()).unwrap();

        let trigrams = Trigram::extract_unique(b"zzzzz");
        let result = reader.search_trigrams(&trigrams);
        assert!(result.is_empty());
    }

    #[test]
    fn roundtrip_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.idx");
        let builder = IndexBuilder::new();
        let built = builder.build();
        write_index(&built, &path).unwrap();

        let reader = IndexReader::open(&path, "empty".into()).unwrap();
        assert_eq!(reader.doc_count(), 0);
        assert_eq!(reader.trigram_count(), 0);
    }

    #[test]
    fn v3_inline_postings() {
        // Create an index where some trigrams appear in ≤3 docs (inline)
        // and others appear in >3 docs (offset/serialized).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inline.idx");
        let mut builder = IndexBuilder::new();

        // "unique_xyz" appears only in doc 0 → inline (1 doc)
        builder.add_doc("a.rs".into(), b"unique_xyz common".to_vec());
        // "unique_abc" appears only in doc 1 → inline (1 doc)
        builder.add_doc("b.rs".into(), b"unique_abc common".to_vec());
        // "common" appears in docs 0,1,2 → inline (3 docs)
        builder.add_doc("c.rs".into(), b"common data here".to_vec());
        // Add 2 more docs with "common" to push it past the inline threshold
        builder.add_doc("d.rs".into(), b"common data also".to_vec());
        builder.add_doc("e.rs".into(), b"common data five".to_vec());

        let built = builder.build();
        write_index(&built, &path).unwrap();

        let reader = IndexReader::open(&path, "test".into()).unwrap();
        assert_eq!(reader.doc_count(), 5);

        // "unique_xyz" trigrams should find only doc 0 (inline path)
        let trigrams = Trigram::extract_unique(b"unique_xyz");
        let result = reader.search_trigrams(&trigrams);
        assert!(result.contains(0));
        assert_eq!(result.len(), 1);

        // "common" trigrams should find all 5 docs (offset path for "com", "omm", "mmo", "mon")
        let trigrams = Trigram::extract_unique(b"common");
        let result = reader.search_trigrams(&trigrams);
        assert_eq!(result.len(), 5);
    }

    #[test]
    fn v3_large_path() {
        // Paths longer than 65535 bytes were impossible with u16 path_len.
        // v3 uses u32 path_len, so this should work.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large_path.idx");
        let mut builder = IndexBuilder::new();

        let long_path = "a/".repeat(40000) + "file.rs"; // ~80005 bytes, > u16::MAX
        builder.add_doc(long_path.clone(), b"fn main() {}".to_vec());

        let built = builder.build();
        write_index(&built, &path).unwrap();

        let reader = IndexReader::open(&path, "test".into()).unwrap();
        assert_eq!(reader.doc_count(), 1);
        assert_eq!(reader.doc_path(0), Some(long_path.as_str()));
        assert_eq!(reader.doc_content(0), Some(b"fn main() {}".as_slice()));
    }

    #[test]
    fn mixed_inline_offset_search() {
        // Full search exercising both inline and offset trigrams together.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.idx");
        let mut builder = IndexBuilder::new();

        // "foobar" in 2 docs (inline trigrams for "foo", "oob", "oba", "bar")
        builder.add_doc("x.rs".into(), b"foobar qux".to_vec());
        builder.add_doc("y.rs".into(), b"foobar baz".to_vec());
        // Add 4 more docs with "qux" to make its trigrams offset-based
        builder.add_doc("q1.rs".into(), b"qux stuff1".to_vec());
        builder.add_doc("q2.rs".into(), b"qux stuff2".to_vec());
        builder.add_doc("q3.rs".into(), b"qux stuff3".to_vec());
        builder.add_doc("q4.rs".into(), b"qux stuff4".to_vec());

        let built = builder.build();
        write_index(&built, &path).unwrap();

        let reader = IndexReader::open(&path, "test".into()).unwrap();

        // Search "foobar qux" — should find only doc 0 (x.rs)
        // This exercises intersection of inline trigrams (from "foobar")
        // with potentially offset trigrams (from "qux").
        let trigrams = Trigram::extract_unique(b"foobar qux");
        let result = reader.search_trigrams(&trigrams);
        assert_eq!(result.len(), 1);
        assert!(result.contains(0));
    }
}
