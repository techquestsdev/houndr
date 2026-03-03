use criterion::{black_box, criterion_group, criterion_main, Criterion};
use houndr_index::query::QueryPlan;
use houndr_index::writer::write_index;
use houndr_index::{IndexBuilder, IndexReader, Trigram};

fn generate_source_file(idx: usize) -> (String, Vec<u8>) {
    let path = format!("src/module_{}/file_{}.rs", idx / 10, idx);
    let content = format!(
        r#"use std::collections::HashMap;

/// Module {} documentation
pub struct Handler{} {{
    name: String,
    count: usize,
    data: HashMap<String, Vec<u8>>,
}}

impl Handler{} {{
    pub fn new(name: &str) -> Self {{
        Self {{
            name: name.to_string(),
            count: 0,
            data: HashMap::new(),
        }}
    }}

    pub fn process(&mut self, input: &[u8]) -> Result<Vec<u8>, std::io::Error> {{
        self.count += 1;
        let key = format!("key_{{}}", self.count);
        self.data.insert(key, input.to_vec());
        Ok(input.to_vec())
    }}

    pub fn get_name(&self) -> &str {{
        &self.name
    }}

    pub fn get_count(&self) -> usize {{
        self.count
    }}
}}

#[cfg(test)]
mod tests {{
    use super::*;

    #[test]
    fn test_handler_new() {{
        let h = Handler{}::new("test");
        assert_eq!(h.get_name(), "test");
        assert_eq!(h.get_count(), 0);
    }}

    #[test]
    fn test_handler_process() {{
        let mut h = Handler{}::new("test");
        let result = h.process(b"hello").unwrap();
        assert_eq!(result, b"hello");
        assert_eq!(h.get_count(), 1);
    }}
}}
"#,
        idx, idx, idx, idx, idx
    );
    (path, content.into_bytes())
}

fn build_test_index(num_files: usize) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bench.idx");

    let mut builder = IndexBuilder::new();
    for i in 0..num_files {
        let (p, c) = generate_source_file(i);
        builder.add_doc(p, c);
    }
    let built = builder.build();
    write_index(&built, &path).unwrap();

    (dir, path)
}

fn bench_build_100_files(c: &mut Criterion) {
    let files: Vec<_> = (0..100).map(generate_source_file).collect();
    c.bench_function("build_100_files", |b| {
        b.iter(|| {
            let mut builder = IndexBuilder::new();
            for (p, content) in &files {
                builder.add_doc(p.clone(), content.clone());
            }
            black_box(builder.build());
        });
    });
}

fn bench_build_1000_files(c: &mut Criterion) {
    let files: Vec<_> = (0..1000).map(generate_source_file).collect();
    c.bench_function("build_1000_files", |b| {
        b.iter(|| {
            let mut builder = IndexBuilder::new();
            for (p, content) in &files {
                builder.add_doc(p.clone(), content.clone());
            }
            black_box(builder.build());
        });
    });
}

fn bench_trigram_extract(c: &mut Criterion) {
    let (_, content) = generate_source_file(0);
    c.bench_function("trigram_extract", |b| {
        b.iter(|| {
            black_box(Trigram::extract_unique(&content));
        });
    });
}

fn bench_search_literal(c: &mut Criterion) {
    let (_dir, path) = build_test_index(500);
    let reader = IndexReader::open(&path, "bench".into()).unwrap();
    let plan = QueryPlan::new("HashMap", false, false).unwrap();

    c.bench_function("search_literal", |b| {
        b.iter(|| {
            black_box(reader.search_trigrams(plan.trigrams()));
        });
    });
}

fn bench_search_rare_literal(c: &mut Criterion) {
    let (_dir, path) = build_test_index(500);
    let reader = IndexReader::open(&path, "bench".into()).unwrap();
    let plan = QueryPlan::new("zzzznotfound", false, false).unwrap();

    c.bench_function("search_rare_literal", |b| {
        b.iter(|| {
            black_box(reader.search_trigrams(plan.trigrams()));
        });
    });
}

fn bench_write_index_500(c: &mut Criterion) {
    let files: Vec<_> = (0..500).map(generate_source_file).collect();
    let dir = tempfile::tempdir().unwrap();

    c.bench_function("write_index_500", |b| {
        b.iter(|| {
            let mut builder = IndexBuilder::new();
            for (p, content) in &files {
                builder.add_doc(p.clone(), content.clone());
            }
            let built = builder.build();
            let path = dir.path().join("bench.idx");
            write_index(&built, &path).unwrap();
        });
    });
}

fn bench_search_inline_trigrams(c: &mut Criterion) {
    // Build an index where the search term appears in ≤3 docs (inline postings)
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bench.idx");
    let mut builder = IndexBuilder::new();
    builder.add_doc("rare.rs".into(), b"unique_rare_xyz code".to_vec());
    builder.add_doc("rare2.rs".into(), b"unique_rare_xyz more".to_vec());
    // Add many other docs with different content
    for i in 0..498 {
        let (p, content) = generate_source_file(i);
        builder.add_doc(p, content);
    }
    let built = builder.build();
    write_index(&built, &path).unwrap();
    let reader = IndexReader::open(&path, "bench".into()).unwrap();
    let plan = QueryPlan::new("unique_rare_xyz", false, false).unwrap();

    c.bench_function("search_inline_trigrams", |b| {
        b.iter(|| {
            black_box(reader.search_trigrams(plan.trigrams()));
        });
    });
}

fn bench_search_mixed_inline_offset(c: &mut Criterion) {
    // Mix of inline and offset trigrams in the same search
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bench.idx");
    let mut builder = IndexBuilder::new();
    // "HashMap" appears in all 500 files (offset postings)
    // "module_42" appears in few files (inline postings)
    for i in 0..500 {
        let (p, content) = generate_source_file(i);
        builder.add_doc(p, content);
    }
    let built = builder.build();
    write_index(&built, &path).unwrap();
    let reader = IndexReader::open(&path, "bench".into()).unwrap();
    let plan = QueryPlan::new("module_42", false, false).unwrap();

    c.bench_function("search_mixed_inline_offset", |b| {
        b.iter(|| {
            black_box(reader.search_trigrams(plan.trigrams()));
        });
    });
}

criterion_group!(
    benches,
    bench_build_100_files,
    bench_build_1000_files,
    bench_trigram_extract,
    bench_search_literal,
    bench_search_rare_literal,
    bench_write_index_500,
    bench_search_inline_trigrams,
    bench_search_mixed_inline_offset,
);
criterion_main!(benches);
