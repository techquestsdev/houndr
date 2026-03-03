use crate::trigram::Trigram;
use rayon::prelude::*;
use roaring::RoaringBitmap;
use rustc_hash::FxHashMap;

/// A document to be indexed.
#[derive(Debug, Clone)]
pub struct DocEntry {
    /// File path relative to the repository root.
    pub path: String,
    /// Raw file content.
    pub content: Vec<u8>,
}

/// Builds an in-memory trigram index from a set of documents.
pub struct IndexBuilder {
    docs: Vec<DocEntry>,
}

impl IndexBuilder {
    /// Create a new empty index builder.
    pub fn new() -> Self {
        Self { docs: Vec::new() }
    }

    /// Add a document. Returns the assigned doc ID.
    pub fn add_doc(&mut self, path: String, content: Vec<u8>) -> u32 {
        let id = u32::try_from(self.docs.len()).expect("index exceeds u32::MAX documents");
        self.docs.push(DocEntry { path, content });
        id
    }

    /// Build the trigram -> bitmap map and return it along with the doc list.
    /// Uses rayon for parallel trigram extraction across documents.
    pub fn build(self) -> BuiltIndex {
        let postings = self
            .docs
            .par_iter()
            .enumerate()
            .fold(
                FxHashMap::<Trigram, RoaringBitmap>::default,
                |mut map, (doc_id, doc)| {
                    let doc_id = doc_id as u32;
                    let trigrams = Trigram::extract_unique(&doc.content);
                    for trigram in trigrams {
                        map.entry(trigram).or_default().insert(doc_id);
                    }
                    map
                },
            )
            .reduce(FxHashMap::default, |mut a, b| {
                for (trigram, bitmap) in b {
                    *a.entry(trigram).or_default() |= &bitmap;
                }
                a
            });

        // Sort trigrams for binary search at read time
        let mut sorted_trigrams: Vec<(Trigram, RoaringBitmap)> = postings.into_iter().collect();
        sorted_trigrams.sort_by_key(|(t, _)| *t);

        BuiltIndex {
            docs: self.docs,
            postings: sorted_trigrams,
        }
    }
}

impl Default for IndexBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// The result of building an index: docs + sorted trigram postings.
pub struct BuiltIndex {
    /// Documents added to the index.
    pub docs: Vec<DocEntry>,
    /// Sorted trigram-to-bitmap posting lists.
    pub postings: Vec<(Trigram, RoaringBitmap)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_empty() {
        let builder = IndexBuilder::new();
        let built = builder.build();
        assert!(built.docs.is_empty());
        assert!(built.postings.is_empty());
    }

    #[test]
    fn build_single_doc() {
        let mut builder = IndexBuilder::new();
        builder.add_doc("test.rs".into(), b"hello world".to_vec());
        let built = builder.build();
        assert_eq!(built.docs.len(), 1);
        assert!(!built.postings.is_empty());

        // "hel" should be a trigram pointing to doc 0
        let hel = Trigram::new(b'h', b'e', b'l');
        let entry = built.postings.iter().find(|(t, _)| *t == hel);
        assert!(entry.is_some());
        assert!(entry.unwrap().1.contains(0));
    }

    #[test]
    fn build_multiple_docs() {
        let mut builder = IndexBuilder::new();
        builder.add_doc("a.rs".into(), b"foo bar".to_vec());
        builder.add_doc("b.rs".into(), b"bar baz".to_vec());
        let built = builder.build();

        // "bar" trigram should be in both docs
        let bar = Trigram::new(b'b', b'a', b'r');
        let entry = built.postings.iter().find(|(t, _)| *t == bar).unwrap();
        assert!(entry.1.contains(0));
        assert!(entry.1.contains(1));

        // "foo" trigram should only be in doc 0
        let foo = Trigram::new(b'f', b'o', b'o');
        let entry = built.postings.iter().find(|(t, _)| *t == foo).unwrap();
        assert!(entry.1.contains(0));
        assert!(!entry.1.contains(1));
    }

    #[test]
    fn postings_sorted() {
        let mut builder = IndexBuilder::new();
        builder.add_doc("test.rs".into(), b"zyxwvu abcdef".to_vec());
        let built = builder.build();

        // Verify postings are sorted by trigram value
        for window in built.postings.windows(2) {
            assert!(window[0].0 < window[1].0);
        }
    }
}
