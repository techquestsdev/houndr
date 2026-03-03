use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use regex_syntax::hir::{Hir, HirKind};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Maximum compiled regex size (bytes) to prevent ReDoS.
const REGEX_SIZE_LIMIT: usize = 1024 * 1024; // 1 MB

/// Maximum matches per file to prevent OOM on pathological queries.
const MAX_MATCHES_PER_FILE: usize = 1000;

use crate::reader::IndexReader;
use crate::trigram::Trigram;

/// Errors that can occur when building a query plan.
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    /// Query is too short to form any trigrams (< 3 characters).
    #[error("query too short: need at least 3 characters to form trigrams")]
    TooShort,
    /// The regex pattern is invalid.
    #[error("invalid regex: {0}")]
    InvalidRegex(String),
    /// No indexable trigrams could be extracted from the regex.
    #[error("no indexable trigrams could be extracted from the regex")]
    NoTrigrams,
}

/// Represents the search plan derived from a user query.
#[derive(Debug)]
pub enum QueryPlan {
    /// Literal substring search — extract trigrams directly from the query string.
    Literal {
        /// The literal search string.
        pattern: String,
        /// Trigrams extracted from the pattern.
        trigrams: Vec<Trigram>,
    },
    /// Regex search — extract literal fragments from the regex AST, then trigrams from those.
    Regex {
        /// The original regex pattern string.
        pattern: String,
        /// Compiled regex for content verification.
        compiled: Regex,
        /// Trigrams extracted from literal fragments of the regex.
        trigrams: Vec<Trigram>,
    },
}

impl QueryPlan {
    /// Create a query plan from a user query string.
    /// If `is_regex` is true, treat it as a regex pattern.
    pub fn new(query: &str, is_regex: bool, case_insensitive: bool) -> Result<Self, QueryError> {
        if is_regex {
            Self::from_regex(query, case_insensitive)
        } else {
            Self::from_literal(query, case_insensitive)
        }
    }

    fn from_literal(query: &str, case_insensitive: bool) -> Result<Self, QueryError> {
        let effective = if case_insensitive {
            query.to_lowercase()
        } else {
            query.to_string()
        };

        let trigrams = Trigram::extract_unique(effective.as_bytes());
        if trigrams.is_empty() {
            return Err(QueryError::TooShort);
        }

        Ok(QueryPlan::Literal {
            pattern: effective,
            trigrams,
        })
    }

    fn from_regex(pattern: &str, case_insensitive: bool) -> Result<Self, QueryError> {
        let full_pattern = if case_insensitive {
            format!("(?i){}", pattern)
        } else {
            pattern.to_string()
        };

        let compiled = RegexBuilder::new(&full_pattern)
            .size_limit(REGEX_SIZE_LIMIT)
            .build()
            .map_err(|e| QueryError::InvalidRegex(e.to_string()))?;

        // Parse regex AST to extract literal fragments
        let hir = regex_syntax::parse(&full_pattern)
            .map_err(|e| QueryError::InvalidRegex(e.to_string()))?;

        let literals = extract_literals(&hir);

        // Extract trigrams from all literal fragments
        let mut all_trigrams = Vec::new();
        for lit in &literals {
            let trigrams = if case_insensitive {
                Trigram::extract_unique(lit.to_lowercase().as_bytes())
            } else {
                Trigram::extract_unique(lit.as_bytes())
            };
            all_trigrams.extend(trigrams);
        }

        // Deduplicate
        all_trigrams.sort();
        all_trigrams.dedup();

        if all_trigrams.is_empty() {
            return Err(QueryError::NoTrigrams);
        }

        Ok(QueryPlan::Regex {
            pattern: full_pattern,
            compiled,
            trigrams: all_trigrams,
        })
    }

    /// Get the trigrams for this query plan.
    pub fn trigrams(&self) -> &[Trigram] {
        match self {
            QueryPlan::Literal { trigrams, .. } => trigrams,
            QueryPlan::Regex { trigrams, .. } => trigrams,
        }
    }
}

/// A line in a result block — either a match or context.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LineMatch {
    /// 1-based line number in the file.
    pub line_number: usize,
    /// Full line content.
    pub line: String,
    /// Character (Unicode scalar) offset pairs `(start, end)` of matches within the line. Empty for context lines.
    pub match_ranges: Vec<(usize, usize)>,
}

/// A contiguous block of lines (matches + surrounding context).
#[derive(Debug, Clone, serde::Serialize)]
pub struct MatchBlock {
    /// Lines in this block, including both match and context lines.
    pub lines: Vec<LineMatch>,
}

/// A file with matching blocks.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileMatch {
    /// File path relative to the repository root.
    pub path: String,
    /// Number of matching lines in this file.
    pub match_count: usize,
    /// Groups of contiguous matching/context lines.
    pub blocks: Vec<MatchBlock>,
}

/// Search results for a single repository.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchResult {
    /// Repository name.
    pub repo: String,
    /// Git clone URL (from config).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Branch or tag that was indexed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// Files with matches in this repository (capped to `max_results`).
    pub files: Vec<FileMatch>,
    /// Total number of files with matches (may exceed `files.len()` when truncated).
    pub total_file_count: usize,
    /// Total number of matching lines across all files (including those not in `files`).
    pub total_match_count: usize,
}

/// Execute a query plan against an index reader, verifying candidates against
/// embedded file content (zero-copy from the mmap).
pub fn execute_search(
    reader: &IndexReader,
    plan: &QueryPlan,
    max_results: usize,
    file_pattern: Option<&glob::Pattern>,
    case_insensitive: bool,
) -> SearchResult {
    // Step 1: Find candidate doc IDs via trigram intersection
    let candidates = reader.search_trigrams(plan.trigrams());
    let candidate_ids: Vec<u32> = candidates.iter().collect();

    // Step 2: Verify candidates in parallel.
    // Build full FileMatch objects (with blocks) for the first max_results files.
    // Continue counting all remaining matches for accurate totals.
    let found_count = AtomicUsize::new(0);
    let overflow_file_count = AtomicUsize::new(0);
    let overflow_match_count = AtomicUsize::new(0);

    let mut files: Vec<FileMatch> = candidate_ids
        .par_iter()
        .filter_map(|&doc_id| {
            let path = reader.doc_path(doc_id)?.to_string();

            // Apply file path filter
            if let Some(pat) = file_pattern {
                if !pat.matches(&path) {
                    return None;
                }
            }

            let content = reader.doc_content(doc_id)?;
            let content_str = std::str::from_utf8(content).ok()?;

            let match_line_numbers = match plan {
                QueryPlan::Literal { pattern, .. } => {
                    find_literal_matches(content_str, pattern, case_insensitive)
                }
                QueryPlan::Regex { compiled, .. } => find_regex_matches(content_str, compiled),
            };

            if match_line_numbers.is_empty() {
                return None;
            }

            let prev = found_count.fetch_add(1, Ordering::Relaxed);
            if prev >= max_results {
                // Past the detail limit — just count, don't build blocks
                overflow_file_count.fetch_add(1, Ordering::Relaxed);
                let occ_count: usize = match_line_numbers
                    .iter()
                    .map(|m| m.match_ranges.len())
                    .sum();
                overflow_match_count.fetch_add(occ_count, Ordering::Relaxed);
                return None;
            }

            let match_count: usize = match_line_numbers
                .iter()
                .map(|m| m.match_ranges.len())
                .sum();
            let all_lines: Vec<&str> = content_str.lines().collect();
            let blocks = build_blocks_with_context(&all_lines, &match_line_numbers, 2);
            Some(FileMatch {
                path,
                match_count,
                blocks,
            })
        })
        .collect();

    // Atomic counter may overshoot slightly due to races — truncate
    files.truncate(max_results);

    let detail_match_count: usize = files.iter().map(|f| f.match_count).sum();
    let total_file_count = files.len() + overflow_file_count.load(Ordering::Relaxed);
    let total_match_count = detail_match_count + overflow_match_count.load(Ordering::Relaxed);

    SearchResult {
        repo: reader.repo_name.clone(),
        url: None,
        git_ref: None,
        files,
        total_file_count,
        total_match_count,
    }
}

fn find_literal_matches(content: &str, pattern: &str, case_insensitive: bool) -> Vec<LineMatch> {
    let mut matches = Vec::new();

    if case_insensitive {
        let needle = pattern.to_lowercase();
        let needle_char_len = needle.chars().count();
        for (line_number, line) in content.lines().enumerate() {
            if matches.len() >= MAX_MATCHES_PER_FILE {
                break;
            }
            let haystack = line.to_lowercase();
            let mut ranges = Vec::new();
            let mut start = 0;
            while let Some(pos) = haystack[start..].find(&needle) {
                let abs_pos = start + pos;
                // Convert byte offset in lowercased string to char offset.
                // to_lowercase() preserves char count for virtually all source-code
                // characters, so char offsets map directly to the original line.
                let char_start = haystack[..abs_pos].chars().count();
                let char_end = char_start + needle_char_len;
                ranges.push((char_start, char_end));
                start = abs_pos + needle.len();
            }
            if !ranges.is_empty() {
                matches.push(LineMatch {
                    line_number: line_number + 1,
                    line: line.to_string(),
                    match_ranges: ranges,
                });
            }
        }
    } else {
        let pattern_char_len = pattern.chars().count();
        for (line_number, line) in content.lines().enumerate() {
            if matches.len() >= MAX_MATCHES_PER_FILE {
                break;
            }
            let mut ranges = Vec::new();
            let mut start = 0;
            while let Some(pos) = line[start..].find(pattern) {
                let abs_pos = start + pos;
                let char_start = line[..abs_pos].chars().count();
                let char_end = char_start + pattern_char_len;
                ranges.push((char_start, char_end));
                start = abs_pos + pattern.len();
            }
            if !ranges.is_empty() {
                matches.push(LineMatch {
                    line_number: line_number + 1,
                    line: line.to_string(),
                    match_ranges: ranges,
                });
            }
        }
    }

    matches
}

fn find_regex_matches(content: &str, regex: &Regex) -> Vec<LineMatch> {
    let mut matches = Vec::new();
    for (line_number, line) in content.lines().enumerate() {
        if matches.len() >= MAX_MATCHES_PER_FILE {
            break;
        }
        let ranges: Vec<(usize, usize)> = regex
            .find_iter(line)
            .map(|m| {
                let char_start = line[..m.start()].chars().count();
                let char_end = char_start + line[m.start()..m.end()].chars().count();
                (char_start, char_end)
            })
            .collect();

        if !ranges.is_empty() {
            matches.push(LineMatch {
                line_number: line_number + 1,
                line: line.to_string(),
                match_ranges: ranges,
            });
        }
    }
    matches
}

/// Build contiguous blocks of lines (matches + context), merging overlapping ranges.
fn build_blocks_with_context(
    all_lines: &[&str],
    matches: &[LineMatch],
    context: usize,
) -> Vec<MatchBlock> {
    let total_lines = all_lines.len();
    if matches.is_empty() {
        return Vec::new();
    }

    // Build a map of match line numbers to their match_ranges for quick lookup
    let mut match_map: std::collections::HashMap<usize, &Vec<(usize, usize)>> =
        std::collections::HashMap::new();
    for m in matches {
        match_map.insert(m.line_number, &m.match_ranges);
    }

    // Compute ranges (start_line..=end_line) for each match with context, then merge overlapping
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for m in matches {
        let start = m.line_number.saturating_sub(context).max(1);
        let end = (m.line_number + context).min(total_lines);
        if let Some(last) = ranges.last_mut() {
            if start <= last.1 + 1 {
                // Merge with previous range
                last.1 = last.1.max(end);
                continue;
            }
        }
        ranges.push((start, end));
    }

    // Build blocks from merged ranges
    let mut blocks = Vec::new();
    for (start, end) in ranges {
        let mut lines = Vec::new();
        for line_num in start..=end {
            let idx = line_num - 1; // 0-indexed
            let line_text = all_lines.get(idx).unwrap_or(&"").to_string();
            let match_ranges = match_map
                .get(&line_num)
                .map(|r| (*r).clone())
                .unwrap_or_default();
            lines.push(LineMatch {
                line_number: line_num,
                line: line_text,
                match_ranges,
            });
        }
        blocks.push(MatchBlock { lines });
    }

    blocks
}

/// Extract literal string fragments from a regex HIR.
fn extract_literals(hir: &Hir) -> Vec<String> {
    let mut literals = Vec::new();
    collect_literals(hir, &mut literals);
    literals
}

fn collect_literals(hir: &Hir, out: &mut Vec<String>) {
    match hir.kind() {
        HirKind::Literal(lit) => {
            if let Ok(s) = std::str::from_utf8(&lit.0) {
                if s.len() >= 3 {
                    out.push(s.to_string());
                }
            }
        }
        HirKind::Concat(exprs) => {
            // Try to concatenate adjacent literals
            let mut current = String::new();
            for expr in exprs {
                if let HirKind::Literal(lit) = expr.kind() {
                    if let Ok(s) = std::str::from_utf8(&lit.0) {
                        current.push_str(s);
                    }
                } else {
                    if current.len() >= 3 {
                        out.push(current.clone());
                    }
                    current.clear();
                    collect_literals(expr, out);
                }
            }
            if current.len() >= 3 {
                out.push(current);
            }
        }
        HirKind::Alternation(exprs) => {
            for expr in exprs {
                collect_literals(expr, out);
            }
        }
        HirKind::Capture(cap) => {
            collect_literals(&cap.sub, out);
        }
        HirKind::Repetition(rep) => {
            collect_literals(&rep.sub, out);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_plan() {
        let plan = QueryPlan::new("hello", false, false).unwrap();
        match &plan {
            QueryPlan::Literal { pattern, trigrams } => {
                assert_eq!(pattern, "hello");
                assert_eq!(trigrams.len(), 3); // hel, ell, llo
            }
            _ => panic!("expected literal plan"),
        }
    }

    #[test]
    fn literal_too_short() {
        let result = QueryPlan::new("ab", false, false);
        assert!(matches!(result, Err(QueryError::TooShort)));
    }

    #[test]
    fn regex_plan() {
        let plan = QueryPlan::new("hello.*world", true, false).unwrap();
        match &plan {
            QueryPlan::Regex { trigrams, .. } => {
                assert!(!trigrams.is_empty());
            }
            _ => panic!("expected regex plan"),
        }
    }

    #[test]
    fn regex_no_literals() {
        let result = QueryPlan::new(".*", true, false);
        assert!(matches!(result, Err(QueryError::NoTrigrams)));
    }

    #[test]
    fn case_insensitive_literal() {
        let plan = QueryPlan::new("Hello", false, true).unwrap();
        match &plan {
            QueryPlan::Literal { pattern, .. } => {
                assert_eq!(pattern, "hello"); // lowercased
            }
            _ => panic!("expected literal plan"),
        }
    }

    #[test]
    fn find_literal_matches_basic() {
        let content = "line one\nhello world\nline three\nhello again";
        let matches = find_literal_matches(content, "hello", false);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].line_number, 2);
        assert_eq!(matches[1].line_number, 4);
    }

    #[test]
    fn find_regex_matches_basic() {
        let content = "fn main() {}\nfn test() {}\nstruct Foo;";
        let re = Regex::new(r"fn \w+").unwrap();
        let matches = find_regex_matches(content, &re);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].line_number, 1);
        assert_eq!(matches[1].line_number, 2);
    }

    #[test]
    fn blocks_with_context_separate() {
        // Lines 1-10, match on line 2 and line 9 — should produce 2 separate blocks
        let lines: Vec<&str> = (0..10).map(|_| "some line").collect();
        let matches = vec![
            LineMatch {
                line_number: 2,
                line: "match".into(),
                match_ranges: vec![(0, 5)],
            },
            LineMatch {
                line_number: 9,
                line: "match".into(),
                match_ranges: vec![(0, 5)],
            },
        ];
        let blocks = build_blocks_with_context(&lines, &matches, 2);
        assert_eq!(blocks.len(), 2);
        // First block: lines 1-4 (match at 2, context 1,3,4)
        assert_eq!(blocks[0].lines.first().unwrap().line_number, 1);
        assert_eq!(blocks[0].lines.last().unwrap().line_number, 4);
        // Second block: lines 7-10 (match at 9, context 7,8,10)
        assert_eq!(blocks[1].lines.first().unwrap().line_number, 7);
        assert_eq!(blocks[1].lines.last().unwrap().line_number, 10);
    }

    #[test]
    fn blocks_with_context_merged() {
        // Lines 1-10, matches on lines 3 and 5 — context overlaps, should merge into 1 block
        let lines: Vec<&str> = (0..10).map(|_| "some line").collect();
        let matches = vec![
            LineMatch {
                line_number: 3,
                line: "match".into(),
                match_ranges: vec![(0, 5)],
            },
            LineMatch {
                line_number: 5,
                line: "match".into(),
                match_ranges: vec![(0, 5)],
            },
        ];
        let blocks = build_blocks_with_context(&lines, &matches, 2);
        assert_eq!(blocks.len(), 1);
        // Block spans lines 1-7
        assert_eq!(blocks[0].lines.first().unwrap().line_number, 1);
        assert_eq!(blocks[0].lines.last().unwrap().line_number, 7);
        // Context lines have empty match_ranges
        assert!(blocks[0].lines[0].match_ranges.is_empty()); // line 1 = context
        assert!(!blocks[0].lines[2].match_ranges.is_empty()); // line 3 = match
    }

    #[test]
    fn blocks_consecutive_matches() {
        // Lines 1-6, matches on 3,4,5 — all consecutive, single block
        let lines: Vec<&str> = (0..6).map(|_| "x").collect();
        let matches = vec![
            LineMatch {
                line_number: 3,
                line: "x".into(),
                match_ranges: vec![(0, 1)],
            },
            LineMatch {
                line_number: 4,
                line: "x".into(),
                match_ranges: vec![(0, 1)],
            },
            LineMatch {
                line_number: 5,
                line: "x".into(),
                match_ranges: vec![(0, 1)],
            },
        ];
        let blocks = build_blocks_with_context(&lines, &matches, 2);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].lines.first().unwrap().line_number, 1);
        assert_eq!(blocks[0].lines.last().unwrap().line_number, 6);
    }

    #[test]
    fn literal_match_ranges_are_char_offsets() {
        // "αβγ" are 2-byte UTF-8 chars; match_ranges must be char offsets, not byte offsets
        let content = "αβγ tetsa δε";
        let matches = find_literal_matches(content, "tetsa", false);
        assert_eq!(matches.len(), 1);
        // "αβγ " = 4 chars, so "tetsa" starts at char 4
        assert_eq!(matches[0].match_ranges, vec![(4, 9)]);

        // Verify the char offsets actually select the right substring
        let line: Vec<char> = matches[0].line.chars().collect();
        let (start, end) = matches[0].match_ranges[0];
        let highlighted: String = line[start..end].iter().collect();
        assert_eq!(highlighted, "tetsa");
    }

    #[test]
    fn literal_match_case_insensitive_unicode() {
        let content = "Χρήστης tetsa end";
        let matches = find_literal_matches(content, "tetsa", true);
        assert_eq!(matches.len(), 1);
        let line: Vec<char> = matches[0].line.chars().collect();
        let (start, end) = matches[0].match_ranges[0];
        let highlighted: String = line[start..end].iter().collect();
        assert_eq!(highlighted, "tetsa");
    }

    #[test]
    fn regex_match_ranges_are_char_offsets() {
        let content = "αβγ foo123 δε";
        let re = Regex::new(r"foo\d+").unwrap();
        let matches = find_regex_matches(content, &re);
        assert_eq!(matches.len(), 1);
        let line: Vec<char> = matches[0].line.chars().collect();
        let (start, end) = matches[0].match_ranges[0];
        let highlighted: String = line[start..end].iter().collect();
        assert_eq!(highlighted, "foo123");
    }
}
