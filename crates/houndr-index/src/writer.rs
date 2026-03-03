use std::fs;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use xxhash_rust::xxh3::Xxh3;

use crate::builder::BuiltIndex;

/// On-disk index format (v3):
///
/// HEADER (64 bytes):
///   magic:           [u8; 4]  = "HNDR"
///   version:         u32      = 3
///   doc_count:       u32
///   trigram_count:   u32
///   doc_table_off:   u64
///   path_data_off:   u64
///   trigram_idx_off: u64
///   posting_off:     u64
///   content_off:     u64
///   _reserved:       [u8; 4]
///
/// DOC TABLE (doc_count × 24-byte entries, naturally aligned):
///   path_offset:     u32  (offset 0, relative to path_data_off)
///   path_len:        u32  (offset 4, widened from u16)
///   content_offset:  u64  (offset 8, relative to content_off)
///   content_len:     u64  (offset 16, widened from u32)
///
/// PATH STRINGS:
///   concatenated UTF-8 paths
///
/// TRIGRAM INDEX (trigram_count × 16-byte entries, sorted by lower 24 bits):
///   u32 word 0:  bits [0:23]  = trigram value
///                bit 24       = inline flag (1=inline, 0=offset)
///                bits [25:26] = inline count - 1 (0..2 → 1..3 doc IDs)
///                bits [27:31] = reserved
///   bytes 4-15:  12-byte payload
///     Inline:  up to 3 × u32 doc IDs (unused slots zeroed)
///     Offset:  posting_offset(u64) + posting_len(u32)
///
/// POSTING DATA:
///   concatenated serialized Roaring bitmaps (only for non-inline entries)
///
/// CONTENT DATA:
///   concatenated raw file contents
///
/// FOOTER (8 bytes):
///   checksum:        u64  (xxhash of everything before footer)
const MAGIC: &[u8; 4] = b"HNDR";
const VERSION: u32 = 3;
const HEADER_SIZE: u64 = 64;

/// Serialize a [`BuiltIndex`] to a binary `.idx` file with atomic rename.
pub fn write_index(index: &BuiltIndex, path: &Path) -> io::Result<()> {
    // Write to a temp file, then rename for atomicity
    let tmp_path = path.with_extension("tmp");
    let file = fs::File::create(&tmp_path)?;
    let mut f = BufWriter::new(file);
    let mut hasher = Xxh3::new();

    // -- Phase 1: Write placeholder header --
    let header_placeholder = [0u8; HEADER_SIZE as usize];
    f.write_all(&header_placeholder)?;

    // -- Phase 2: Write doc table --
    let doc_table_off = HEADER_SIZE;
    let mut path_buf = Vec::new();
    let mut content_offset: u64 = 0;

    for doc in &index.docs {
        let path_offset = u32::try_from(path_buf.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "path data exceeds u32::MAX bytes",
            )
        })?;
        let path_len = u32::try_from(doc.path.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "single path exceeds u32::MAX bytes",
            )
        })?;
        let content_len = doc.content.len() as u64;

        f.write_all(&path_offset.to_le_bytes())?; // offset 0
        f.write_all(&path_len.to_le_bytes())?; // offset 4
        f.write_all(&content_offset.to_le_bytes())?; // offset 8
        f.write_all(&content_len.to_le_bytes())?; // offset 16

        path_buf.extend_from_slice(doc.path.as_bytes());
        content_offset += content_len;
    }

    // -- Phase 3: Write path strings --
    let path_data_off = doc_table_off + (index.docs.len() as u64 * DOC_ENTRY_SIZE as u64);
    f.write_all(&path_buf)?;

    // -- Phase 4: Serialize posting bitmaps (only for non-inline entries) --
    // Inline threshold: trigrams with ≤3 doc IDs are stored inline.
    let mut posting_blobs: Vec<Option<Vec<u8>>> = Vec::with_capacity(index.postings.len());
    for (_, bitmap) in &index.postings {
        if bitmap.len() <= 3 {
            posting_blobs.push(None); // inline — no blob needed
        } else {
            let mut buf = Vec::new();
            bitmap.serialize_into(&mut buf)?;
            posting_blobs.push(Some(buf));
        }
    }

    // -- Phase 5: Write trigram index --
    let trigram_idx_off = path_data_off + path_buf.len() as u64;
    let mut posting_offset: u64 = 0;

    for (i, (trigram, bitmap)) in index.postings.iter().enumerate() {
        if bitmap.len() <= 3 {
            // Inline mode: pack flags into upper byte
            let count = bitmap.len() as u32;
            let flags_word = trigram.0
                | (1u32 << 24)                    // inline flag
                | (((count - 1) & 0x3) << 25); // count - 1 in bits 25:26
            f.write_all(&flags_word.to_le_bytes())?;

            // Write up to 3 doc IDs, pad remaining with zero
            let doc_ids: Vec<u32> = bitmap.iter().collect();
            for j in 0..3 {
                let id = if j < doc_ids.len() { doc_ids[j] } else { 0 };
                f.write_all(&id.to_le_bytes())?;
            }
        } else {
            // Offset mode: upper bits = 0
            f.write_all(&trigram.0.to_le_bytes())?;
            let blob = posting_blobs[i].as_ref().unwrap();
            let posting_len = blob.len() as u32;
            f.write_all(&posting_offset.to_le_bytes())?;
            f.write_all(&posting_len.to_le_bytes())?;
            posting_offset += posting_len as u64;
        }
    }

    // -- Phase 6: Write posting data (only non-inline blobs) --
    let posting_off = trigram_idx_off + (index.postings.len() as u64 * TRIGRAM_ENTRY_SIZE as u64);
    for data in posting_blobs.iter().flatten() {
        f.write_all(data)?;
    }

    // -- Phase 7: Write content data --
    let content_off = posting_off + posting_offset;
    for doc in &index.docs {
        f.write_all(&doc.content)?;
    }

    // Flush the BufWriter before seeking
    f.flush()?;
    let mut f = f.into_inner().map_err(|e| e.into_error())?;

    // -- Phase 8: Write real header --
    f.seek(SeekFrom::Start(0))?;

    let mut header = Vec::with_capacity(HEADER_SIZE as usize);
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&VERSION.to_le_bytes());
    let doc_count = u32::try_from(index.docs.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "doc count exceeds u32::MAX"))?;
    let trigram_count = u32::try_from(index.postings.len()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "trigram count exceeds u32::MAX")
    })?;
    header.extend_from_slice(&doc_count.to_le_bytes());
    header.extend_from_slice(&trigram_count.to_le_bytes());
    header.extend_from_slice(&doc_table_off.to_le_bytes());
    header.extend_from_slice(&path_data_off.to_le_bytes());
    header.extend_from_slice(&trigram_idx_off.to_le_bytes());
    header.extend_from_slice(&posting_off.to_le_bytes());
    header.extend_from_slice(&content_off.to_le_bytes());
    // Reserved (pad to 64 bytes)
    header.resize(HEADER_SIZE as usize, 0);

    f.write_all(&header)?;
    f.flush()?;
    drop(f);

    // -- Phase 9: Compute checksum via streaming read --
    {
        use std::io::Read;
        let mut file = fs::File::open(&tmp_path)?;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
    }
    let checksum = hasher.digest();

    // Append footer
    let mut f = fs::OpenOptions::new().append(true).open(&tmp_path)?;
    f.write_all(&checksum.to_le_bytes())?;
    f.flush()?;
    drop(f);

    // Atomic swap
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Size of each doc table entry in bytes (v3: naturally aligned).
/// path_offset(u32) + path_len(u32) + content_offset(u64) + content_len(u64)
pub const DOC_ENTRY_SIZE: usize = 4 + 4 + 8 + 8;

/// Size of each trigram index entry in bytes.
/// flags_trigram(u32) + 12-byte payload (inline doc IDs or offset+len)
pub const TRIGRAM_ENTRY_SIZE: usize = 4 + 12;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::IndexBuilder;

    #[test]
    fn write_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.idx");
        let builder = IndexBuilder::new();
        let built = builder.build();
        write_index(&built, &path).unwrap();
        assert!(path.exists());
        let data = fs::read(&path).unwrap();
        assert_eq!(&data[0..4], b"HNDR");
    }

    #[test]
    fn write_index_with_docs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.idx");
        let mut builder = IndexBuilder::new();
        builder.add_doc("hello.rs".into(), b"fn main() {}".to_vec());
        builder.add_doc("world.rs".into(), b"fn test() {}".to_vec());
        let built = builder.build();
        write_index(&built, &path).unwrap();
        assert!(path.exists());
    }
}
